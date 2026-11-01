//! Minimal owned raw ISDB-S MPEG-TS capture graph. No CAS or descrambling.
use crate::{
    gr_i2c, snapshot,
    tssrc::{TsSource, XcReaderSource},
    tuner,
    xcdrv::{TsiMappingSnapshot, XcDrv},
};
use std::{
    io,
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
fn err(s: impl Into<String>) -> io::Error {
    io::Error::other(s.into())
}
fn stage<T>(name: &str, result: io::Result<T>) -> io::Result<T> {
    result.map_err(|e| stage_error(name, e))
}
fn stage_error(name: &str, e: io::Error) -> io::Error {
    io::Error::new(
        e.kind(),
        format!("{name}: {e} (raw_os_error={:?})", e.raw_os_error()),
    )
}
fn should_restore_tsi(touched: bool, graph_clear: bool, cleanup_ok: bool) -> bool {
    touched && graph_clear && cleanup_ok
}
fn identity() -> io::Result<()> {
    for (path, want) in [
        ("/usr/bin/tuner", "0416f546e87bd2b0c7a2e764ea0ee292"),
        ("/usr/bin/abs", "def4436239553ede33f0febf86eab318"),
        ("/disk/xc6drv/xcdrv.ko", "3db75958a29ecbb880c5a658584556d3"),
        (
            "/usr/vixs/lib/libvixs.so",
            "27ed7eda99d63977a5989b9bdba7e571",
        ),
    ] {
        let r = Command::new("md5sum").arg(path).output()?;
        if !r.status.success()
            || !String::from_utf8_lossy(&r.stdout).starts_with(&format!("{want} "))
        {
            return Err(err(format!("unverified OEM image: {path}")));
        }
    }
    Ok(())
}
/// Recover the current frontend mode on server restart, without retuning.
/// tuner:aDevOfPath[1] is the FEM's real path1 assignment, not cached SI.
pub fn legacy_frontend() -> io::Result<bool> {
    identity()?;
    let pid = snapshot::procmem::find_pid("tuner")?;
    snapshot::procmem::verify_layout(pid)?;
    let bytes = snapshot::procmem::read(pid, tuner::A_DEV_OF_PATH_ADDR + 4, 4)?;
    match u32::from_le_bytes(bytes.try_into().unwrap()) {
        2 => Ok(true),
        0x200 | 1 => Ok(false),
        _ => Err(err("unexpected FEM path1 assignment")),
    }
}

pub struct Source {
    drv: Arc<XcDrv>,
    mapping: Option<TsiMappingSnapshot>,
    pipeline: u32,
    av: u32,
    device: u32,
    buffer: u32,
    connection: u32,
    filter: bool,
    running: bool,
    tsi_may_be_modified: bool,
    reader: Option<XcReaderSource>,
    scratch: Vec<u8>,
    last_data: Instant,
    dirty: Arc<AtomicBool>,
    label: String,
}
impl Source {
    pub fn open(
        drv: Arc<XcDrv>,
        freq: u32,
        tsid: u16,
        legacy: &AtomicBool,
        dirty: Arc<AtomicBool>,
    ) -> io::Result<Self> {
        if dirty.load(Ordering::SeqCst) {
            return Err(err(
                "previous graph cleanup failed; inspect before restarting",
            ));
        }
        stage("OEM identity", identity())?;
        if stage("frontend assignment preflight", legacy_frontend())? {
            return Err(err(
                "frontend is already assigned to legacy ISDB-S; refusing takeover",
            ));
        }
        let mapping = stage("TSI mapping snapshot", drv.snapshot_tsi_mapping())?;
        let mut source = Self {
            drv,
            mapping: Some(mapping),
            pipeline: 0,
            av: 0,
            device: 0,
            buffer: 0,
            connection: 0,
            filter: false,
            running: false,
            tsi_may_be_modified: false,
            reader: None,
            scratch: Vec::new(),
            last_data: Instant::now(),
            dirty,
            label: format!("ISDB-S RF {freq} TSID {tsid:04X} / raw encrypted MPEG-TS"),
        };
        let req = tuner::FedTuneRequest::isdb_s(1, tsid, freq)
            .ok_or_else(|| err("invalid ISDB-S path"))?;
        req.validate().map_err(err)?;
        legacy.store(true, Ordering::SeqCst); // includes partial frontend changes
        stage(
            "open FEM message queue",
            tuner::Tuner::open(tuner::MSGH_LIST),
        )?
        .set_device(&tuner::FemDeviceRequest::assign(1, 2))
        .map_err(|e| stage_error("assign FEM path1 to ISDB-S", e))?;
        std::thread::sleep(Duration::from_millis(300));
        stage(
            "open FED message queue",
            tuner::Tuner::open_qid(req.fed_qid, tuner::MSGH_LIST),
        )?
        .tune_fed(&req)
        .map_err(|e| stage_error("post ISDB-S FED tune request", e))?;
        let until = Instant::now() + Duration::from_secs(12);
        loop {
            let r = stage("read SC1501A output status", gr_i2c::read_output_regs())?;
            if r.lock & 0x20 != 0
                && r.bank2_selection[..2] == tsid.to_be_bytes()
                && r.bank0.iter().any(|&(r, v)| r == 0xcc && v & 0x80 != 0)
                && r.bank7.iter().any(|&(r, v)| r == 0x72 && v == 0)
            {
                break;
            }
            if Instant::now() >= until {
                return Err(err(
                    "ISDB-S lock/output/TSID did not match requested channel",
                ));
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        source.pipeline = stage("create owned pipeline", source.drv.create_pipeline())?;
        source.av = stage("create owned AV_SYNC", source.drv.create_av_sync())?;
        stage(
            "attach AV_SYNC to pipeline",
            source.drv.set_pipeline(source.av, source.pipeline),
        )?;
        // Both calls can mutate hardware and then report an error; mark the
        // snapshot as potentially dirty before the first one.
        source.tsi_may_be_modified = true;
        stage(
            "configure physical TSI0",
            source.drv.configure_product_mpeg_ts_tsi(0),
        )?;
        stage(
            "map logical TSI1 to physical TSI0",
            source.drv.map_tsi_one_to_zero(),
        )?;
        stage(
            "set AV_SYNC live mode",
            source.drv.set_av_sync_live_mode(source.av),
        )?;
        source.device = stage(
            "create owned MPEG-TS DEMUX",
            source.drv.create_mpeg_ts_demux(),
        )?;
        stage(
            "attach DEMUX to pipeline",
            source.drv.set_pipeline(source.device, source.pipeline),
        )?;
        stage(
            "configure DEMUX recorder input TSI1",
            source.drv.config_ts_recorder_tsi(source.device, 1),
        )?;
        // A failed RUN may still have partially started the owned device.
        source.running = true;
        stage(
            "run owned DEMUX",
            source.drv.run_product_demux(source.device),
        )?;
        source.buffer = stage("create owned TS ring", source.drv.create_demux_ts_buffer())?;
        source.connection = stage(
            "connect owned DEMUX output",
            source.drv.connect_output(source.device, source.buffer),
        )?;
        source.reader = Some(stage(
            "create owned ring reader",
            XcReaderSource::attach(Arc::clone(&source.drv), source.buffer, 200),
        )?);
        // ADD_PID may install the filter before reporting an error.
        source.filter = true;
        stage(
            "add full-TS PID filter",
            source
                .drv
                .add_demux_pid(source.device, u32::MAX, source.connection),
        )?;
        stage(
            "set TS ring ready",
            source.drv.set_buffer_ready(source.buffer),
        )?;
        source.last_data = Instant::now();
        eprintln!("mkra: opened {}", source.label);
        Ok(source)
    }
}
impl TsSource for Source {
    fn read_into(&mut self, out: &mut Vec<u8>) -> io::Result<usize> {
        let before = out.len();
        self.scratch.clear();
        let result = (|| {
            self.reader.as_mut().unwrap().read_into(&mut self.scratch)?;
            if !self.scratch.is_empty() {
                self.last_data = Instant::now();
                out.extend_from_slice(&self.scratch);
            } else if self.last_data.elapsed() > Duration::from_secs(8) {
                return Err(err("BS2K input stopped for eight seconds"));
            }
            Ok(out.len() - before)
        })();
        result.map_err(|e| {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("BS2K session ended: {e}"),
            )
        })
    }
    fn describe(&self) -> String {
        self.label.clone()
    }
}
impl Drop for Source {
    fn drop(&mut self) {
        let mut good = true;
        macro_rules! check {
            ($label:expr, $result:expr $(,)?) => {
                if let Err(e) = $result {
                    good = false;
                    eprintln!("mkra: BS2K cleanup {}: {e}", $label);
                }
            };
        }
        if self.filter {
            check!(
                "remove PID",
                self.drv.remove_demux_pid(self.device, u32::MAX),
            );
        }
        if let Some(mut reader) = self.reader.take() {
            check!("release owned reader", reader.close());
        }
        if self.connection != 0 {
            if let Err(first) = self
                .drv
                .disconnect_product_demux(self.device, self.connection)
            {
                eprintln!("mkra: BS2K disconnect needs stop/retry: {first}");
                let mut retry_allowed = true;
                if self.running {
                    match self.drv.stop_device(self.device) {
                        Ok(()) => self.running = false,
                        Err(e) => {
                            good = false;
                            retry_allowed = false;
                            eprintln!("mkra: BS2K cleanup stop retry: {e}");
                        }
                    }
                }
                if retry_allowed {
                    check!(
                        "disconnect retry",
                        self.drv
                            .disconnect_product_demux(self.device, self.connection),
                    );
                }
            }
        }
        if self.running {
            match self.drv.stop_device(self.device) {
                Ok(()) => self.running = false,
                Err(e) => {
                    good = false;
                    eprintln!("mkra: BS2K cleanup stop: {e}");
                }
            }
        }
        let clear = self.buffer == 0
            || self
                .drv
                .sbuf_status_raw(self.buffer)
                .is_ok_and(|s| s[8] == 0 && s[9] == 0);
        if self.buffer != 0 {
            if clear && !self.running {
                check!("reset ring", self.drv.force_reset_buffer(self.buffer));
                check!("destroy ring", self.drv.destroy_buffer(self.buffer));
            } else {
                check!(
                    "ring ownership",
                    Err::<(), _>(err("retaining graph with live writer/readers")),
                );
            }
        }
        let can_destroy_graph = clear && !self.running;
        if self.device != 0 && can_destroy_graph {
            check!("destroy DEMUX", self.drv.destroy_device(self.device));
        }
        if self.pipeline != 0 && can_destroy_graph {
            check!("stop pipeline", self.drv.stop_pipeline(self.pipeline));
        }
        if self.av != 0 && can_destroy_graph {
            check!("destroy AV_SYNC", self.drv.destroy_device(self.av));
        }
        if self.pipeline != 0 && can_destroy_graph {
            check!("destroy pipeline", self.drv.destroy_pipeline(self.pipeline));
        }
        if should_restore_tsi(self.tsi_may_be_modified, can_destroy_graph, good) {
            if let Some(mapping) = self.mapping.take() {
                check!("restore TSI", self.drv.restore_tsi_mapping(&mapping));
            }
        } else if self.tsi_may_be_modified {
            eprintln!(
                "mkra: BS2K cleanup: retaining TSI mapping because graph cleanup is incomplete"
            );
        }
        if !good {
            self.dirty.store(true, Ordering::SeqCst);
        }
        eprintln!("mkra: BS2K raw graph closed; cleanup_ok={good}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tsi_restore_is_skipped_before_any_tsi_mutation() {
        assert!(!should_restore_tsi(false, true, true));
    }

    #[test]
    fn partial_tsi_mutation_is_restored_only_after_clean_owned_graph_teardown() {
        assert!(should_restore_tsi(true, true, true));
        assert!(!should_restore_tsi(true, false, true));
        assert!(!should_restore_tsi(true, true, false));
    }

    #[test]
    fn failing_ioctl_diagnostic_identifies_stage_and_preserves_raw_code_text() {
        let e = stage::<()>(
            "ioctl 0x53 logical TSI1 restore",
            Err(io::Error::from_raw_os_error(1032)),
        )
        .unwrap_err();
        assert!(e.to_string().contains("ioctl 0x53 logical TSI1 restore"));
        assert!(e.to_string().contains("raw_os_error=Some(1032)"));
    }
}
/// Restore native frontend initialization. Actual 4K service tune follows in factory.
pub fn restore_frontend() -> io::Result<()> {
    identity()?;
    gr_i2c::quiesce_ts_output()?;
    tuner::Tuner::open(tuner::MSGH_LIST)?.set_device(&tuner::FemDeviceRequest::assign(1, 0x200))?;
    std::thread::sleep(Duration::from_millis(300));
    let pid = snapshot::procmem::find_pid("tuner")?;
    snapshot::procmem::verify_layout(pid)?;
    for addr in [snapshot::RECOMMEND_FLAGS, snapshot::PSEQ_FLAGS] {
        let bytes = snapshot::procmem::read(pid, addr, 4)?;
        match u32::from_le_bytes(bytes.try_into().unwrap()) {
            0 => {}
            1 => {
                snapshot::oneshot::clear(pid, addr)?;
            }
            _ => return Err(err("unexpected tuner initialization flag")),
        }
    }
    Ok(())
}
