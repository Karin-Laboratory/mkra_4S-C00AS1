//! The Mirakurun-compatible surface.
//!
//! Shapes follow Mirakurun's own `api.yml` definitions and the behaviour of its
//! route handlers (`Content-Type: video/MP2T`, the
//! `X-Mirakurun-Tuner-User-ID` response header, `X-Mirakurun-Priority` on the
//! way in, `decode` as a query parameter, HEAD support, 503 when no tuner is
//! free). DESIGN.md §2 lists what is implemented and what is deliberately not.

use std::io::Write;
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crate::channels::{Channel, ChannelTable, Service};
use crate::http::{
    respond_error, respond_json, respond_stream_headers, Request,
};
use crate::json::{self, Obj};
use crate::si::{Program, SiDb};
use crate::stream::{Broker, SubscribeError};
use crate::sys;

/// Reported by `/api/version` as `current`.
///
/// Clients gate features on the Mirakurun version they think they are talking
/// to, so we report a Mirakurun release we are actually shape-compatible with
/// and put mkra's own version in `/api/status`.
pub const COMPAT_MIRAKURUN_VERSION: &str = "3.9.0";
pub const MKRA_VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long a stream request waits for its first byte before giving the tuner
/// back and answering 503. Generous enough to cover a tune plus lock
/// (`--tune-settle-ms` defaults to 1.5 s) and a slow first chunk.
const FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long an established stream may go silent before we conclude the
/// frontend has stopped and release it.
const STALL_TIMEOUT: Duration = Duration::from_secs(15);

pub struct AppState {
    pub table: ChannelTable,
    pub broker: Arc<Broker>,
    pub started: Instant,
    pub requests: AtomicU64,
    pub tuning_enabled: bool,
    pub backend: String,
    /// EPG parsed from the product's own MMT-SI cache. Empty until
    /// `--epg-dir` is given and the box has actually acquired a schedule.
    pub epg: RwLock<SiDb>,
}

impl AppState {
    pub fn new(
        table: ChannelTable,
        broker: Arc<Broker>,
        tuning_enabled: bool,
        backend: String,
    ) -> Self {
        AppState {
            table,
            broker,
            started: Instant::now(),
            requests: AtomicU64::new(0),
            tuning_enabled,
            backend,
            epg: RwLock::new(SiDb::new()),
        }
    }

    /// The `ChannelType` this instance advertises, taken from the channel table
    /// so it can never disagree with what `/api/channels` reports.
    pub fn tuner_type(&self) -> &str {
        self.table
            .channels
            .first()
            .map(|c| c.ch_type())
            .unwrap_or(crate::channels::DEFAULT_CHANNEL_TYPE)
    }

    /// Re-read the product's SI cache.
    ///
    /// `adsb` rewrites these files as it acquires tables, so this is called on
    /// a timer rather than once at startup. A whole rebuild is cheap compared
    /// with tracking per-file versions, and it means a stale EPG can never
    /// outlive the file it came from.
    ///
    /// The 8.4 MB schedule slab is the only large read; everything else is
    /// 8-40 KB. Returns the number of programs after the rebuild.
    pub fn reload_epg(&self, dir: &str) -> std::io::Result<usize> {
        // Order matters only in that the NIT and SDT give names the programs
        // are looked up against; a full rebuild makes it moot.
        const FILES: [&str; 7] = [
            "AdsbSiTlvNitActual.dat",
            "AdsbSiMhSdtActual.dat",
            "AdsbSiMhSdtOther.dat",
            "AdsbSiMhTot.dat",
            "AdsbSiMhEitPF.dat",
            "AdsbSiMhEitSch.dat",
            // Extended schedule (table_id 0x94..0x9B) carries the events the
            // basic slab does not; without it the guide has holes.
            "AdsbSiMhEitSchEx.dat",
        ];
        let mut db = SiDb::new();
        let mut read_any = false;
        for f in FILES {
            let path = format!("{}/{}", dir.trim_end_matches('/'), f);
            match std::fs::read(&path) {
                Ok(bytes) => {
                    db.ingest(&bytes);
                    read_any = true;
                }
                // A missing or unreadable table is normal: the box only writes
                // one once it has received it.
                Err(_) => continue,
            }
        }
        if !read_any {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no Adsb*.dat readable under {dir}"),
            ));
        }
        let n = db.programs.len();
        if let Ok(mut guard) = self.epg.write() {
            *guard = db;
        }
        Ok(n)
    }

    /// Merge SI read straight off the driver's section rings into the EPG.
    ///
    /// Unlike [`reload_epg`](Self::reload_epg) this accumulates rather than
    /// rebuilds: a ring only holds the last ~36 KB of sections, so each pass
    /// contributes a slice of the schedule and dropping what came before would
    /// throw the guide away every cycle. Sections are keyed by programme id, so
    /// a repeat is an update, not a duplicate.
    ///
    /// Returns (sections accepted this pass, total programs known).
    pub fn merge_ring_si(&self, chunks: &[Vec<u8>]) -> (usize, usize) {
        let Ok(mut db) = self.epg.write() else {
            return (0, 0);
        };
        let mut accepted = 0;
        for c in chunks {
            accepted += db.ingest_ring(c);
        }
        (accepted, db.programs.len())
    }
}

// --- JSON shapes ---------------------------------------------------------

fn write_service(o: &mut Obj, s: &Service, ch: Option<&Channel>) {
    o.u64("id", s.item_id())
        .num("serviceId", s.service_id as i64)
        .num("networkId", s.network_id as i64)
        .str("name", &s.name)
        .num("type", s.service_type as i64);
    if let Some(c) = ch {
        o.obj("channel", |c2| {
            c2.str("type", c.ch_type()).str("channel", &c.key);
        });
    }
}

fn write_channel(o: &mut Obj, c: &Channel, with_services: bool) {
    o.str("type", c.ch_type())
        .str("channel", &c.key)
        .str("name", &c.name);
    if with_services {
        o.arr("services", |a| {
            for s in &c.services {
                a.obj(|o| write_service(o, s, None));
            }
        });
    }
}

/// One Mirakurun `Program`.
///
/// Field names and units follow Mirakurun's schema: `startAt`/`duration` are
/// milliseconds, `extended` is an object of caption→text pairs, and `genres`
/// carries the raw ARIB nibbles rather than names so clients can apply their
/// own genre tables.
fn write_program(o: &mut Obj, p: &Program) {
    o.u64("id", p.id)
        .num("eventId", p.event_id as i64)
        .num("serviceId", p.service_id as i64)
        .num("networkId", p.network_id as i64)
        .num("startAt", p.start_at_ms)
        .num("duration", p.duration_ms)
        .bool("isFree", p.is_free);
    if !p.name.is_empty() {
        o.str("name", &p.name);
    }
    if !p.description.is_empty() {
        o.str("description", &p.description);
    }
    if !p.extended.is_empty() {
        o.obj("extended", |e| {
            for (k, v) in &p.extended {
                e.str(k, v);
            }
        });
    }
    if !p.genres.is_empty() {
        o.arr("genres", |a| {
            for g in &p.genres {
                a.obj(|go| {
                    go.num("lv1", g.lv1 as i64)
                        .num("lv2", g.lv2 as i64)
                        .num("un1", g.un1 as i64)
                        .num("un2", g.un2 as i64);
                });
            }
        });
    }
    if let Some(v) = &p.video {
        o.obj("video", |vo| {
            // Advanced BS is HEVC everywhere; the descriptor has no codec
            // field, so this is a property of the delivery system, not a guess
            // about this particular event.
            //
            // Mirakurun's `componentType` is the ARIB byte that packs
            // resolution/aspect/frame-rate together — MMT-SI splits those into
            // separate fields and has no such byte, so it is omitted rather
            // than invented. `componentTag` is real and is reported instead.
            vo.str("type", "h.265")
                .num("streamContent", 1)
                .num("componentTag", v.component_tag as i64)
                .bool("progressive", v.progressive);
            if let Some(r) = v.resolution {
                vo.str("resolution", r);
            }
            if let Some(a) = v.aspect {
                vo.str("aspect", a);
            }
            if let Some(t) = v.transfer_characteristics {
                vo.str("transferCharacteristics", t);
            }
        });
    }
    if !p.audios.is_empty() {
        o.arr("audios", |a| {
            for au in &p.audios {
                a.obj(|ao| {
                    ao.num("componentType", au.component_type as i64)
                        .num("componentTag", au.component_tag as i64)
                        .bool("isMain", au.is_main);
                    if let Some(sr) = au.sampling_rate {
                        ao.num("samplingRate", sr as i64);
                    }
                    ao.arr("langs", |l| {
                        for lang in &au.langs {
                            l.str(lang);
                        }
                    });
                });
            }
        });
    }
}

fn channels_json(list: &[&Channel]) -> String {
    json::array(|a| {
        for c in list {
            a.obj(|o| write_channel(o, c, true));
        }
    })
}

fn services_json(list: &[(&Channel, &Service)]) -> String {
    json::array(|a| {
        for (c, s) in list {
            a.obj(|o| write_service(o, s, Some(c)));
        }
    })
}

/// One `TunerDevice`. The box has a single usable path as far as we can prove,
/// so exactly one is reported; see DESIGN.md §3.
fn tuners_json(state: &AppState) -> String {
    let session = state.broker.current();
    let using = session.is_some();
    json::array(|a| {
        a.obj(|o| {
            o.num("index", 0)
                .str("name", "ADBS0")
                // Must follow --channel-type: advertising BS4K channels while
                // claiming the only tuner serves "BS" makes any client that
                // replicates Mirakurun's tuner-selection logic conclude there
                // is no usable tuner.
                .arr("types", |t| {
                    let types:std::collections::BTreeSet<_>=state.table.channels.iter().map(|c|c.ch_type()).collect();
                    for kind in types {t.str(kind);}
                })
                // Mirakurun spawns an external command per tuner; mkra reads
                // the driver in-process, so there is nothing to report.
                .str("command", "")
                .num("pid", std::process::id() as i64)
                .arr("users", |u| {
                    if let Some(sess) = &session {
                        for sub in sess.subscribers() {
                            let (packets, drops) = sub.counters();
                            u.obj(|o| {
                                o.str("id", &sub.id)
                                    .num("priority", sub.priority as i64)
                                    .str("agent", &sub.agent)
                                    .str("url", &sub.url)
                                    .obj("streamInfo", |si| {
                                        si.obj(&sess.channel_key, |p| {
                                            p.u64("packet", packets).u64("drop", drops);
                                        });
                                    });
                            });
                        }
                    }
                })
                .bool("isAvailable", true)
                .bool("isRemote", false)
                .bool("isFree", !using)
                .bool("isUsing", using)
                .bool("isFault", false);
        });
    })
}

fn status_json(state: &AppState) -> String {
    let session = state.broker.current();
    let streams = session.as_ref().map(|s| s.subscribers().len()).unwrap_or(0);
    let errors = session
        .as_ref()
        .map(|s| s.errors.load(Ordering::Relaxed))
        .unwrap_or(0);

    json::object(|o| {
        o.u64("time", sys::unix_time_ms())
            .str("version", COMPAT_MIRAKURUN_VERSION)
            .obj("process", |p| {
                p.str("arch", std::env::consts::ARCH)
                    .str("platform", std::env::consts::OS)
                    .obj("versions", |v| {
                        v.str("mkra", MKRA_VERSION);
                    })
                    .obj("env", |_| {})
                    .num("pid", std::process::id() as i64)
                    .obj("memoryUsage", |m| {
                        // Only rss is meaningful without a managed heap.
                        m.u64("rss", sys::rss_bytes())
                            .num("heapTotal", 0)
                            .num("heapUsed", 0);
                    });
            })
            .obj("epg", |e| {
                // No EPG source yet; see DESIGN.md §9.
                e.arr("gatheringNetworks", |_| {}).num("storedEvents", 0);
            })
            .u64("rpcCount", state.requests.load(Ordering::Relaxed))
            .obj("streamCount", |s| {
                s.num("tunerDevice", if session.is_some() { 1 } else { 0 })
                    .num("tsFilter", streams as i64)
                    .num("decoder", 0);
            })
            .obj("errorCount", |e| {
                e.num("uncaughtException", 0)
                    .num("unhandledRejection", 0)
                    .u64("bufferOverflow", total_drops(state))
                    .u64("tunerDeviceRespawn", errors)
                    .num("decoderRespawn", 0);
            })
            .obj("timerAccuracy", |t| {
                // Mirakurun measures event-loop lag here. There is no event
                // loop; report zeroes rather than invent numbers.
                let zero = |o: &mut Obj| {
                    o.f64("avg", 0.0).f64("min", 0.0).f64("max", 0.0);
                };
                t.f64("last", 0.0)
                    .obj("m1", zero)
                    .obj("m5", zero)
                    .obj("m15", zero);
            })
            // Not in Mirakurun's schema; harmless extra fields, and they make
            // `curl /api/status` enough to diagnose the box.
            .obj("mkra", |m| {
                m.str("version", MKRA_VERSION)
                    .str("backend", &state.backend)
                    .bool("tuningEnabled", state.tuning_enabled)
                    .u64("uptimeSec", state.started.elapsed().as_secs())
                    .raw(
                        "session",
                        &match &session {
                            Some(s) => json::object(|o| {
                                o.str("channel", &s.channel_key)
                                    .str("source", &s.source_desc)
                                    .u64("bytesIn", s.bytes_in.load(Ordering::Relaxed))
                                    .u64("uptimeSec", s.started.elapsed().as_secs());
                            }),
                            None => "null".into(),
                        },
                    );
            });
    })
}

fn total_drops(state: &AppState) -> u64 {
    state
        .broker
        .current()
        .map(|s| s.subscribers().iter().map(|x| x.counters().1).sum())
        .unwrap_or(0)
}

// --- routing -------------------------------------------------------------

/// The outcome of routing a request.
///
/// Routing is a pure function so that tests exercise the same table the server
/// does; only `Stream` needs a real socket.
#[derive(Debug, PartialEq)]
pub enum Route {
    Json(u16, String),
    /// Open-ended TS body for the given channel key.
    Stream(String),
}

pub fn route(state: &AppState, req: &Request) -> Route {
    let json_ok = |body: String| Route::Json(200, body);
    let err = |code: u16, reason: &str| {
        Route::Json(
            code,
            json::object(|o| {
                o.num("code", code as i64).str("reason", reason);
            }),
        )
    };

    if req.method != "GET" && req.method != "HEAD" {
        return err(405, "method not allowed");
    }

    match req.segments().as_slice() {
        [] => json_ok(json::object(|o| {
            o.str("name", "mkra")
                .str("version", MKRA_VERSION)
                .str("api", "/api/version");
        })),

        ["api", "version"] => json_ok(json::object(|o| {
            o.str("current", COMPAT_MIRAKURUN_VERSION)
                .str("latest", COMPAT_MIRAKURUN_VERSION);
        })),

        ["api", "status"] => json_ok(status_json(state)),

        ["api", "tuners"] => json_ok(tuners_json(state)),

        ["api", "tuners", "0"] => {
            // The array form is the schema; a single device is its element.
            let all = tuners_json(state);
            json_ok(
                all.strip_prefix('[')
                    .and_then(|s| s.strip_suffix(']'))
                    .unwrap_or("{}")
                    .to_string(),
            )
        }
        ["api", "tuners", _] => err(404, "tuner not found"),

        ["api", "channels"] => json_ok(channels_json(&filter_channels(state, req))),

        ["api", "channels", ty] => {
            let list: Vec<&Channel> = state
                .table
                .channels
                .iter()
                .filter(|c| c.ch_type().eq_ignore_ascii_case(ty))
                .collect();
            json_ok(channels_json(&list))
        }

        ["api", "channels", ty, ch] => match state.table.find(ty, ch) {
            Some(c) => json_ok(json::object(|o| write_channel(o, c, true))),
            None => err(404, "channel not found"),
        },

        ["api", "channels", ty, ch, "services"] => match state.table.find(ty, ch) {
            Some(c) => {
                let list: Vec<(&Channel, &Service)> = c.services.iter().map(|s| (c, s)).collect();
                json_ok(services_json(&list))
            }
            None => err(404, "channel not found"),
        },

        ["api", "channels", ty, ch, "services", id] => {
            let found = state.table.find(ty, ch).and_then(|c| {
                c.services
                    .iter()
                    // Mirakurun addresses these by bare service id; accept the
                    // full item id too rather than surprising a caller.
                    .find(|s| {
                        id.parse::<u16>().is_ok_and(|v| v == s.service_id)
                            || id.parse::<u64>().is_ok_and(|v| v == s.item_id())
                    })
                    .map(|s| (c, s))
            });
            match found {
                Some((c, s)) => json_ok(json::object(|o| write_service(o, s, Some(c)))),
                None => err(404, "service not found"),
            }
        }

        ["api", "channels", ty, ch, "stream"] => match state.table.find(ty, ch) {
            Some(c) => Route::Stream(c.key.clone()),
            None => err(404, "channel not found"),
        },

        ["api", "services"] => json_ok(services_json(&filter_services(state, req))),

        ["api", "services", id] => {
            match id.parse::<u64>().ok().and_then(|v| state.table.find_service(v)) {
                Some((c, s)) => json_ok(json::object(|o| write_service(o, s, Some(c)))),
                None => err(404, "service not found"),
            }
        }

        ["api", "services", id, "stream"] => {
            match id.parse::<u64>().ok().and_then(|v| state.table.find_service(v)) {
                Some((c, _)) => Route::Stream(c.key.clone()),
                None => err(404, "service not found"),
            }
        }

        // EPG. Empty until --epg-dir points at the product's SI cache and the
        // box has acquired a schedule; an empty list is a truthful answer that
        // keeps clients from erroring out.
        ["api", "programs"] => {
            let db = match state.epg.read() {
                Ok(g) => g,
                Err(_) => return json_ok("[]".into()),
            };
            let want_service = req.query_value("serviceId").and_then(|v| v.parse::<u16>().ok());
            let want_network = req.query_value("networkId").and_then(|v| v.parse::<u16>().ok());
            json_ok(json::array(|a| {
                for p in db.programs.values() {
                    if want_service.is_some_and(|s| s != p.service_id) {
                        continue;
                    }
                    if want_network.is_some_and(|n| n != p.network_id) {
                        continue;
                    }
                    a.obj(|o| write_program(o, p));
                }
            }))
        }

        ["api", "services", id, "programs"] => {
            let Some((_, svc)) = id.parse::<u64>().ok().and_then(|v| state.table.find_service(v))
            else {
                return err(404, "service not found");
            };
            let db = match state.epg.read() {
                Ok(g) => g,
                Err(_) => return json_ok("[]".into()),
            };
            json_ok(json::array(|a| {
                for p in db.programs_of(svc.network_id, svc.service_id) {
                    a.obj(|o| write_program(o, p));
                }
            }))
        }

        ["api", "programs", id] => {
            let Some(want) = id.parse::<u64>().ok() else {
                return err(404, "program not found");
            };
            let db = match state.epg.read() {
                Ok(g) => g,
                Err(_) => return err(404, "program not found"),
            };
            match db.programs.get(&want) {
                Some(p) => json_ok(json::object(|o| write_program(o, p))),
                None => err(404, "program not found"),
            }
        }

        _ => err(404, "not found"),
    }
}

pub fn handle(state: &AppState, req: &Request, out: &mut TcpStream) {
    state.requests.fetch_add(1, Ordering::Relaxed);
    let head_only = req.method == "HEAD";

    let result = match route(state, req) {
        Route::Json(code, body) => respond_json(out, code, &body, head_only),
        Route::Stream(key) => {
            stream(state, req, out, &key, head_only);
            return;
        }
    };

    if let Err(e) = result {
        // A broken pipe here just means the client hung up first.
        if e.kind() != std::io::ErrorKind::BrokenPipe {
            eprintln!("mkra: response failed for {}: {e}", req.path);
        }
    }
}

fn filter_channels<'a>(state: &'a AppState, req: &Request) -> Vec<&'a Channel> {
    state
        .table
        .channels
        .iter()
        .filter(|c| {
            req.query_value("type")
                .is_none_or(|v| c.ch_type().eq_ignore_ascii_case(v))
                && req.query_value("channel").is_none_or(|v| c.key == v)
                && req.query_value("name").is_none_or(|v| c.name.contains(v))
        })
        .collect()
}

fn filter_services<'a>(state: &'a AppState, req: &Request) -> Vec<(&'a Channel, &'a Service)> {
    state
        .table
        .services()
        .filter(|(c, s)| {
            req.query_value("serviceId")
                .is_none_or(|v| v.parse::<u16>().is_ok_and(|x| x == s.service_id))
                && req
                    .query_value("networkId")
                    .is_none_or(|v| v.parse::<u16>().is_ok_and(|x| x == s.network_id))
                && req
                    .query_value("type")
                    .is_none_or(|v| v.parse::<u8>().is_ok_and(|x| x == s.service_type))
                && req.query_value("name").is_none_or(|v| s.name.contains(v))
                && req
                    .query_value("channel.type")
                    .is_none_or(|v| c.ch_type().eq_ignore_ascii_case(v))
        })
        .collect()
}

// --- streaming -----------------------------------------------------------

fn stream(state: &AppState, req: &Request, out: &mut TcpStream, channel_key: &str, head_only: bool) {
    let user_id = format!("{}:{}", req.peer, sys::unix_time_ms());

    if head_only {
        let _ = respond_stream_headers(
            out,
            &[("X-Mirakurun-Tuner-User-ID", user_id)],
        );
        return;
    }

    let priority = req
        .header("X-Mirakurun-Priority")
        .and_then(|v| v.parse::<i32>().ok())
        .unwrap_or(0);

    let subscription = state.broker.subscribe(
        channel_key,
        user_id.clone(),
        priority,
        req.header("User-Agent").unwrap_or("").to_string(),
        req.path.clone(),
    );

    let (session, sub) = match subscription {
        Ok(v) => v,
        Err(SubscribeError::Busy(current)) => {
            let _ = respond_error(
                out,
                503,
                &format!("tuner busy with {current}"),
                false,
            );
            return;
        }
        Err(SubscribeError::TooManyClients) => {
            let _ = respond_error(out, 503, "too many stream clients", false);
            return;
        }
        Err(SubscribeError::Source(e)) => {
            eprintln!("mkra: cannot open TS source for {channel_key}: {e}");
            let _ = respond_error(out, 503, "no TS source available", false);
            return;
        }
    };

    // Wait for the first byte BEFORE committing to a 200.
    //
    // Without this, a backend that is merely silent -- which is what the box
    // looks like whenever no buffer handle has been found, and what NullSource
    // does by design -- produces an open 200 that never delivers anything. The
    // client waits for ever, the session is never released, `/api/tuners`
    // reports the tuner busy, and every other channel gets 503 for the lifetime
    // of the process. A silent tuner has to fail like a busy one, not like a
    // working one.
    let mut buf = vec![0u8; 64 * 1024];
    let mut first = 0usize;
    let deadline = Instant::now() + FIRST_BYTE_TIMEOUT;
    while first == 0 {
        if sub.is_closed() {
            break;
        }
        if Instant::now() >= deadline {
            eprintln!(
                "mkra: no data from the TS backend for {channel_key} within {:?}; \
                 releasing the tuner",
                FIRST_BYTE_TIMEOUT
            );
            state.broker.unsubscribe(&session, &sub);
            let _ = respond_error(out, 503, "tuner produced no data", false);
            return;
        }
        first = sub.read(&mut buf, Duration::from_millis(200));
    }

    if first==0 {
        state.broker.unsubscribe(&session,&sub);
        let _=respond_error(out,503,"TS source ended before producing data",false);return;
    }
    if respond_stream_headers(out, &[("X-Mirakurun-Tuner-User-ID", user_id)]).is_err() {
        state.broker.unsubscribe(&session, &sub);
        return;
    }
    if first > 0 && out.write_all(&buf[..first]).is_err() {
        state.broker.unsubscribe(&session, &sub);
        return;
    }

    // Mid-stream stall: a frontend that loses lock stops producing without
    // erroring, and the same lease-leak applies. Give up after the same budget.
    let mut idle_since = Instant::now();
    loop {
        let n = sub.read(&mut buf, Duration::from_millis(500));
        if n == 0 {
            if sub.is_closed() || idle_since.elapsed() >= STALL_TIMEOUT {
                break;
            }
            continue;
        }
        idle_since = Instant::now();
        if out.write_all(&buf[..n]).is_err() {
            break; // client went away
        }
    }
    state.broker.unsubscribe(&session, &sub);
}

/// Build a `Request` from a raw request line, for tests.
#[cfg(test)]
fn request(path: &str) -> Request {
    use std::io::Cursor;
    let raw = format!("GET {path} HTTP/1.1\r\nHost: t\r\n\r\n");
    let mut c = Cursor::new(raw.into_bytes());
    crate::http::read_request(&mut c, "test:1".into())
        .unwrap()
        .unwrap()
}

/// Route a path and return `(status, body)`. Streaming routes report 200 with
/// the resolved channel key so tests can assert on resolution.
#[cfg(test)]
pub fn render(state: &AppState, path: &str) -> (u16, String) {
    match route(state, &request(path)) {
        Route::Json(code, body) => (code, body),
        Route::Stream(key) => (200, key),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::{Broker, BrokerConfig};
    use crate::tssrc::{NullSource, TsSource};

    fn state() -> AppState {
        let broker = Broker::new(
            BrokerConfig::default(),
            Box::new(|_| Ok(Box::new(NullSource) as Box<dyn TsSource>)),
        );
        AppState::new(ChannelTable::builtin(), broker, false, "null".into())
    }

    /// Minimal structural check: balanced braces/brackets outside strings.
    fn is_balanced(s: &str) -> bool {
        let (mut depth, mut in_str, mut esc) = (0i32, false, false);
        for c in s.chars() {
            if in_str {
                if esc {
                    esc = false;
                } else if c == '\\' {
                    esc = true;
                } else if c == '"' {
                    in_str = false;
                }
                continue;
            }
            match c {
                '"' => in_str = true,
                '{' | '[' => depth += 1,
                '}' | ']' => depth -= 1,
                _ => {}
            }
            if depth < 0 {
                return false;
            }
        }
        depth == 0 && !in_str
    }

    #[test]
    fn version_reports_a_mirakurun_compatible_version() {
        let (code, body) = render(&state(), "/api/version");
        assert_eq!(code, 200);
        assert!(body.contains(r#""current":"3.9.0""#), "{body}");
        assert!(is_balanced(&body));
    }

    #[test]
    fn channels_lists_every_transport_stream_with_services() {
        let (code, body) = render(&state(), "/api/channels");
        assert_eq!(code, 200);
        assert!(is_balanced(&body));
        assert_eq!(body.matches(r#""channel":"#).count(), 10);
        assert!(body.contains(r#""type":"BS""#));
        assert!(body.contains(r#""serviceId":101"#));
    }

    #[test]
    fn channels_can_be_filtered_by_query() {
        let s = state();
        let (_, all) = render(&s, "/api/channels");
        let (_, one) = render(&s, "/api/channels?channel=BS17_0");
        assert!(one.len() < all.len());
        assert_eq!(one.matches(r#""channel":"#).count(), 1);

        let (_, none) = render(&s, "/api/channels?type=GR");
        assert_eq!(none, "[]");
    }

    #[test]
    fn a_single_channel_is_addressable() {
        let s = state();
        let (code, body) = render(&s, "/api/channels/BS/BS17_0");
        assert_eq!(code, 200);
        assert!(body.contains(r#""channel":"BS17_0""#));

        let (code, _) = render(&s, "/api/channels/BS/NOPE");
        assert_eq!(code, 404);
    }

    #[test]
    fn services_carry_their_channel_and_item_id() {
        let (code, body) = render(&state(), "/api/services");
        assert_eq!(code, 200);
        assert!(is_balanced(&body));
        assert!(body.contains(r#""id":1100101"#));
        assert!(body.contains(r#""networkId":11"#));
        assert!(body.contains(r#""channel":{"type":"BS","channel":"#));
    }

    #[test]
    fn service_lookup_by_item_id_works_and_unknown_ids_404() {
        let s = state();
        let (code, body) = render(&s, "/api/services/1100101");
        assert_eq!(code, 200);
        assert!(body.contains(r#""serviceId":101"#));

        let (code, _) = render(&s, "/api/services/9999999");
        assert_eq!(code, 404);
        // A non-numeric id must be a clean 404, not a panic.
        let (code, _) = render(&s, "/api/services/abc");
        assert_eq!(code, 404);
    }

    #[test]
    fn services_filter_by_service_id() {
        let (_, body) = render(&state(), "/api/services?serviceId=101");
        assert_eq!(body.matches(r#""serviceId":"#).count(), 1);
    }

    #[test]
    fn tuners_reports_one_free_device_when_idle() {
        let (code, body) = render(&state(), "/api/tuners");
        assert_eq!(code, 200);
        assert!(is_balanced(&body));
        assert!(body.contains(r#""isFree":true"#));
        assert!(body.contains(r#""isUsing":false"#));
        assert!(body.contains(r#""types":["BS"]"#));
        assert!(body.contains(r#""users":[]"#));
    }

    #[test]
    fn tuner_types_follow_the_configured_channel_type() {
        // A client that replicates Mirakurun's tuner selection needs the tuner
        // to claim the same ChannelType the channels are published under.
        let mut table = ChannelTable::builtin();
        table.set_channel_type(crate::channels::BS4K_CHANNEL_TYPE);
        let s = AppState::new(
            table,
            Broker::new(
                BrokerConfig::default(),
                Box::new(|_| Ok(Box::new(NullSource) as Box<dyn TsSource>)),
            ),
            false,
            "null".into(),
        );
        assert_eq!(s.tuner_type(), "BS4K");
        let (_, body) = render(&s, "/api/tuners");
        assert!(body.contains(r#""types":["BS4K"]"#), "{body}");
        let (_, chans) = render(&s, "/api/channels");
        assert!(chans.contains(r#""type":"BS4K""#));
    }

    #[test]
    fn epg_endpoints_are_empty_but_well_formed_before_any_si_is_loaded() {
        let s = state();
        for path in ["/api/programs", "/api/services/1100101/programs"] {
            let (code, body) = render(&s, path);
            assert_eq!(code, 200, "{path}");
            assert_eq!(body, "[]", "{path}");
        }
        assert_eq!(render(&s, "/api/programs/110010121408").0, 404);
        assert_eq!(render(&s, "/api/services/9999999/programs").0, 404);
    }

    #[test]
    fn epg_endpoints_serve_parsed_programs() {
        let s = state();
        let mut db = crate::si::SiDb::new();
        db.ingest_section(include_bytes!("../testdata/mh_eit_sched.bin"));
        let want = *db.programs.keys().next().unwrap();
        let sid = db.programs[&want].service_id;
        *s.epg.write().unwrap() = db;

        let (code, body) = render(&s, "/api/programs");
        assert_eq!(code, 200);
        assert!(is_balanced(&body));
        assert!(body.contains(&format!(r#""id":{want}"#)), "missing {want}");

        // Filters must not silently ignore an unknown service.
        let (_, none) = render(&s, "/api/programs?serviceId=999");
        assert_eq!(none, "[]");
        let (_, some) = render(&s, &format!("/api/programs?serviceId={sid}"));
        assert!(some.contains(&format!(r#""id":{want}"#)));

        let (code, one) = render(&s, &format!("/api/programs/{want}"));
        assert_eq!(code, 200);
        assert!(one.starts_with('{') && one.contains(r#""startAt":"#));
    }

    #[test]
    fn status_is_well_formed_and_names_the_backend() {
        let (code, body) = render(&state(), "/api/status");
        assert_eq!(code, 200);
        assert!(is_balanced(&body), "{body}");
        assert!(body.contains(r#""backend":"null""#));
        assert!(body.contains(r#""storedEvents":0"#));
        assert!(body.contains(r#""session":null"#));
    }

    #[test]
    fn programs_is_an_empty_list_rather_than_an_error() {
        let (code, body) = render(&state(), "/api/programs");
        assert_eq!(code, 200);
        assert_eq!(body, "[]");
    }

    #[test]
    fn unknown_paths_return_the_mirakurun_error_shape() {
        let (code, body) = render(&state(), "/api/nope");
        assert_eq!(code, 404);
        assert!(body.contains(r#""code":404"#));
        assert!(body.contains(r#""reason""#));
    }

    #[test]
    fn both_stream_routes_resolve_to_the_same_channel() {
        let s = state();
        // A service stream and its channel stream must reach one channel key,
        // otherwise the broker would treat them as competing tunes.
        assert_eq!(
            route(&s, &request("/api/channels/BS/BS17_0/stream")),
            Route::Stream("BS17_0".into())
        );
        assert_eq!(
            route(&s, &request("/api/services/1100101/stream")),
            Route::Stream("BS17_0".into())
        );
    }

    #[test]
    fn streaming_an_unknown_target_is_a_404_not_a_tune() {
        let s = state();
        assert!(matches!(
            route(&s, &request("/api/channels/BS/NOPE/stream")),
            Route::Json(404, _)
        ));
        assert!(matches!(
            route(&s, &request("/api/services/424242/stream")),
            Route::Json(404, _)
        ));
    }

    #[test]
    fn non_get_methods_are_rejected() {
        let s = state();
        let mut req = request("/api/channels");
        req.method = "POST".into();
        assert!(matches!(route(&s, &req), Route::Json(405, _)));
        // HEAD must route exactly like GET; the body is dropped later.
        req.method = "HEAD".into();
        assert!(matches!(route(&s, &req), Route::Json(200, _)));
    }

    #[test]
    fn channel_scoped_service_lookup_accepts_both_id_forms() {
        let s = state();
        let (a, _) = render(&s, "/api/channels/BS/BS17_0/services/101");
        let (b, _) = render(&s, "/api/channels/BS/BS17_0/services/1100101");
        assert_eq!((a, b), (200, 200));
        let (c, _) = render(&s, "/api/channels/BS/BS17_0/services/999");
        assert_eq!(c, 404);
    }

    #[test]
    fn every_advertised_channel_can_be_fetched_and_streamed() {
        // Guards against a channel appearing in the listing under a key that
        // the per-channel routes cannot resolve.
        let s = state();
        for c in &s.table.channels {
            let (code, _) = render(&s, &format!("/api/channels/BS/{}", c.key));
            assert_eq!(code, 200, "channel {} not fetchable", c.key);
            assert_eq!(
                route(&s, &request(&format!("/api/channels/BS/{}/stream", c.key))),
                Route::Stream(c.key.clone())
            );
            for svc in &c.services {
                let (code, _) = render(&s, &format!("/api/services/{}", svc.item_id()));
                assert_eq!(code, 200, "service {} not fetchable", svc.item_id());
            }
        }
    }
}
