//! Synthetic memory-input diagnostic. Creates and destroys only owned objects.
//! It never tunes RF, configures physical TSI, or attaches native ES buffers.
use crate::{
    testts::TestPatternSource,
    tssrc::{TsSource, XcReaderSource},
    xcdrv::{ts_sync_ratio, XcDrv, DEV_PATH},
};
use std::{
    io,
    sync::Arc,
    time::{Duration, Instant},
};

pub enum Case {
    Tsi,
    LocalSmall,
    LocalBulk,
    LocalFile,
    LocalTts,
    LocalPat,
    LocalStarted,
    LocalTtsStarted,
    LocalDescribed,
    LocalTtsDescribed,
    LocalPatDescribed,
}

pub fn run(case: Case) -> i32 {
    let local = !matches!(case, Case::Tsi);
    let described = matches!(
        case,
        Case::LocalDescribed | Case::LocalTtsDescribed | Case::LocalPatDescribed
    );
    let bulk = described
        || matches!(
            case,
            Case::LocalBulk
                | Case::LocalFile
                | Case::LocalTts
                | Case::LocalPat
                | Case::LocalStarted
                | Case::LocalTtsStarted
        );
    let real = described
        || matches!(
            case,
            Case::LocalFile
                | Case::LocalTts
                | Case::LocalPat
                | Case::LocalStarted
                | Case::LocalTtsStarted
        );
    let tts = matches!(
        case,
        Case::LocalTts | Case::LocalTtsStarted | Case::LocalTtsDescribed
    );
    let started = matches!(case, Case::LocalStarted | Case::LocalTtsStarted);
    let pat = matches!(case, Case::LocalPat | Case::LocalPatDescribed);
    let packet = if tts { 192 } else { 188 };
    let drv = match XcDrv::open_path(DEV_PATH) {
        Ok(x) => Arc::new(x),
        Err(e) => {
            eprintln!("loopback: {e}");
            return 1;
        }
    };
    let (
        mut input,
        mut output,
        mut writer,
        mut device,
        mut pipeline,
        mut avsync,
        mut cin,
        mut cout,
    ) = (0, 0, 0, 0, 0, 0, 0, 0);
    let mut filter = false;
    let result = (|| -> io::Result<()> {
        let mut pattern = TestPatternSource::new(0x7abc, 0x7abd, 0x7abe, "MEMORY_TEST", 2_000_000);
        let mut sent = Vec::new();
        pattern.read_into(&mut sent)?;
        if tts {
            sent = vec![0x5a; 192 * 64];
        }
        eprintln!(
            "loopback: SYNTHETIC input={} bytes, not received broadcast",
            sent.len()
        );
        input = if tts {
            drv.create_diagnostic_tts_buffer()?
        } else {
            drv.create_demux_ts_buffer()?
        };
        writer = drv.create_diagnostic_writer(input)?;
        let mut reader = XcReaderSource::attach(Arc::clone(&drv), input, 100)?;
        drv.set_buffer_ready(input)?;
        drv.write_diagnostic_ts(writer, &sent, packet, described)?;
        let mut received = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        while received.len() < sent.len() && Instant::now() < deadline {
            reader.read_into(&mut received)?;
        }
        if received != sent {
            return Err(io::Error::other(format!(
                "buffer self-check mismatch: sent={} read={}",
                sent.len(),
                received.len()
            )));
        }
        eprintln!(
            "loopback: host buffer self-check exact_match=true bytes={}",
            sent.len()
        );
        drop(reader);
        pipeline = drv
            .create_pipeline()
            .map_err(|e| io::Error::new(e.kind(), format!("create pipeline: {e}")))?;
        avsync = drv
            .create_av_sync()
            .map_err(|e| io::Error::new(e.kind(), format!("create AV_SYNC: {e}")))?;
        drv.set_pipeline(avsync, pipeline)?;
        drv.set_av_sync_live_mode(avsync)
            .map_err(|e| io::Error::new(e.kind(), format!("AV_SYNC live: {e}")))?;
        device = if local {
            drv.create_local_ts_demux()
        } else {
            drv.create_memory_ts_demux()
        }
        .map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "create {} DEMUX (max_in=1): {e}",
                    if local { "LOCAL" } else { "TSI" }
                ),
            )
        })?;
        eprintln!(
            "loopback: DEMUX created type={} handle={device:08x}",
            if local { 6 } else { 12 }
        );
        drv.set_pipeline(device, pipeline)?;
        cin = drv
            .connect_input(device, input)
            .map_err(|e| io::Error::new(e.kind(), format!("connect memory input: {e}")))?;
        drv.run_product_demux(device)
            .map_err(|e| io::Error::new(e.kind(), format!("run memory DEMUX: {e}")))?;
        output = if pat {
            drv.create_demux_pat_section_buffer()?
        } else {
            drv.create_demux_ts_buffer()?
        };
        cout = drv.connect_output(device, output)?;
        let mut reader = XcReaderSource::attach(Arc::clone(&drv), output, 100)?;
        (if pat {
            drv.add_demux_pat_section(device, cout)
        } else {
            drv.add_demux_pid(device, u32::MAX, cout)
        })
        .map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("add {} filter: {e}", if pat { "PAT" } else { "full-TS" }),
            )
        })?;
        filter = true;
        drv.set_buffer_ready(output)?;
        eprintln!(
            "loopback: pipeline time before start/feed={:?}",
            drv.pipeline_get_time(pipeline)
        );
        if started {
            drv.start_diagnostic_pipeline(pipeline)
                .map_err(|e| io::Error::new(e.kind(), format!("start own pipeline: {e}")))?;
            eprintln!(
                "loopback: own pipeline start issued; time={:?}",
                drv.pipeline_get_time(pipeline)
            );
        }
        let feed = if real {
            let data = std::fs::read(if tts {
                "/tmp/mt/local-source.tts"
            } else {
                "/tmp/mt/loopback-4k.ts"
            })?;
            let n = data.len().min(packet * 8192) / packet * packet;
            if n < packet * 4
                || data[..n]
                    .chunks_exact(packet)
                    .any(|p| p[if tts { 4 } else { 0 }] != 0x47)
            {
                return Err(io::Error::other(
                    "recorded input has no verified packet sync",
                ));
            }
            data[..n].to_vec()
        } else if bulk {
            let mut data = Vec::new();
            while data.len() < 188 * 8192 {
                data.extend_from_slice(&sent);
            }
            data
        } else {
            sent.clone()
        };
        eprintln!(
            "loopback: memory feed bytes={} origin={}",
            feed.len(),
            if tts {
                "recorded_4K_TTS"
            } else if real {
                "recorded_4K_TS"
            } else {
                "synthetic"
            }
        );
        std::fs::write("/tmp/mt/local-loopback-input.bin", &feed)?;
        for data in feed.chunks(packet * 1024) {
            drv.write_diagnostic_ts(writer, data, packet, described)?;
        }
        let mut received = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        while received.len() < feed.len() && Instant::now() < deadline {
            reader.read_into(&mut received)?;
        }
        std::fs::write("/tmp/mt/local-loopback-output.bin", &received)?;
        eprintln!(
            "loopback: pipeline time after capture={:?}",
            drv.pipeline_get_time(pipeline)
        );
        eprintln!("loopback: input status={:x?}", drv.sbuf_status_raw(input)?);
        eprintln!(
            "loopback: output status={:x?}",
            drv.sbuf_status_raw(output)?
        );
        let expected: std::collections::HashSet<&[u8]> = feed
            .chunks_exact(packet)
            .map(|p| &p[if tts { 4 } else { 0 }..])
            .collect();
        if pat {
            let count = crate::valid_pat_sections(&received);
            println!(
                "loopback: recorded-input PAT output={} valid_sections={count}; not BS2K reception",
                received.len()
            );
            if count == 0 {
                return Err(io::Error::other(
                    "LOCAL_DEMUX returned no CRC-valid PAT section",
                ));
            }
            return Ok(());
        }
        let known =
            !received.is_empty() && received.chunks_exact(188).all(|p| expected.contains(p));
        let sync = ts_sync_ratio(&received);
        println!("loopback: MEMORY DEMUX output={} sync={sync:.4} known_packets={known}; not BS2K reception",received.len());
        if received.len() < 188 * 4 || received.len() % 188 != 0 || sync != 1.0 || !known {
            return Err(io::Error::other("memory DEMUX did not return verified TS"));
        }
        Ok(())
    })();
    let mut errors = Vec::new();
    let mut clean = |label: &str, r: io::Result<()>| {
        if let Err(e) = r {
            errors.push(format!("{label}: {e}"));
        }
    };
    if filter {
        clean(
            "remove PID",
            drv.remove_demux_pid(device, if pat { 0 } else { u32::MAX }),
        );
    }
    if device != 0 {
        clean("stop DEMUX", drv.stop_device(device));
    }
    if cout != 0 {
        clean("disconnect output", drv.disconnect(device, cout));
    }
    if cin != 0 {
        clean("disconnect input", drv.disconnect(device, cin));
    }
    if writer != 0 {
        clean("cancel writer", drv.cancel_get_read(writer));
        clean("destroy writer", drv.destroy_diagnostic_writer(writer));
    }
    if device != 0 {
        clean("destroy DEMUX", drv.destroy_device(device));
    }
    for (label, buffer) in [("output", output), ("input", input)] {
        if buffer != 0 {
            let clear = matches!(drv.sbuf_status_raw(buffer), Ok(s) if s[8] == 0 && s[9] == 0);
            if clear {
                clean(&format!("reset {label}"), drv.force_reset_buffer(buffer));
                clean(&format!("destroy {label}"), drv.destroy_buffer(buffer));
            } else {
                clean(
                    &format!("retain owned {label}"),
                    Err(io::Error::other("writer/readers did not clear")),
                );
            }
        }
    }
    if pipeline != 0 {
        clean("stop pipeline", drv.stop_pipeline(pipeline));
    }
    if avsync != 0 {
        clean("destroy AV_SYNC", drv.destroy_device(avsync));
    }
    if pipeline != 0 {
        clean("destroy pipeline", drv.destroy_pipeline(pipeline));
    }
    for error in &errors {
        eprintln!("loopback cleanup: {error}");
    }
    eprintln!("loopback: owned-object cleanup errors={}", errors.len());
    match result {
        Ok(()) if errors.is_empty() => 0,
        Ok(()) => 1,
        Err(e) => {
            eprintln!("loopback: {e}");
            1
        }
    }
}
