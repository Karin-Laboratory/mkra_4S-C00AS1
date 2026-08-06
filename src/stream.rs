//! Session management: one reader on the driver, many HTTP clients.
//!
//! The box has one usable frontend path as far as we can prove, so the broker
//! deliberately serves **one channel at a time**. A second client asking for
//! the same channel joins the existing pump; a client asking for a different
//! one gets 503, which is exactly what Mirakurun returns when no tuner is
//! free, so clients already handle it.

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::tssrc::TsSource;

pub const TS_PACKET: usize = 188;

/// Smallest per-client ring we will honour. Anything below a few packets makes
/// the drop counter meaningless and starves the writer thread.
pub const MIN_RING: usize = 64 * TS_PACKET;

/// How much unaligned data a new subscriber will discard while hunting for a
/// packet boundary before concluding the stream is not 188-byte TS and passing
/// bytes through untouched.
const RESYNC_GIVE_UP: u64 = 1 << 20;

/// Serve bytes exactly as the source produced them, with no 188-byte hunt.
///
/// The alignment hunt is right for a transport stream and wrong for anything
/// else: a passive tap on one of the driver's section rings carries MMT/TLV
/// sections, which never contain a 188-byte sync pattern, so a client would
/// see nothing at all until `RESYNC_GIVE_UP` bytes had accumulated — about a
/// hundred seconds on a section ring. This makes that a choice instead of a
/// surprise.
pub static RAW_PASSTHROUGH: AtomicBool = AtomicBool::new(false);

/// First index in `data` that looks like the start of a TS packet.
///
/// A lone `0x47` is not enough — it occurs in payload roughly once per 256
/// bytes. Requiring the sync byte to repeat one packet later makes a false
/// positive unlikely, and the caller only needs one good boundary per session.
fn ts_alignment(data: &[u8]) -> Option<usize> {
    for i in 0..data.len() {
        if data[i] != 0x47 {
            continue;
        }
        match data.get(i + TS_PACKET) {
            Some(0x47) => return Some(i),
            // Too near the end to confirm; a later chunk will settle it.
            None => return None,
            _ => {}
        }
    }
    None
}

/// A bounded byte ring with a keep-live overflow policy.
///
/// When a client cannot keep up we discard the *oldest* bytes rather than the
/// newest, so the stream it does receive stays close to real time instead of
/// drifting further behind with every stall. Discards are counted and surface
/// as `streamInfo.drop` on `/api/tuners`.
struct ByteRing {
    buf: Box<[u8]>,
    head: usize,
    len: usize,
    /// Overflow always discards a whole multiple of this. For a TS consumer it
    /// is 188, so a slow client loses packets rather than packet *phase*.
    evict_unit: usize,
}

impl ByteRing {
    /// `cap` is used exactly as given; enforcing a sensible minimum is the
    /// caller's job (see `MIN_RING`), which keeps this a plain data structure.
    /// Byte-granular ring, used by the unit tests and by any consumer that is
    /// not carrying MPEG-TS.
    fn new(cap: usize) -> Self {
        Self::with_evict_unit(cap, 1)
    }

    /// Ring that only ever evicts whole `unit`-sized records.
    fn with_evict_unit(cap: usize, evict_unit: usize) -> Self {
        assert!(cap > 0, "ring capacity must be non-zero");
        assert!(evict_unit > 0, "eviction unit must be non-zero");
        ByteRing {
            evict_unit,
            buf: vec![0u8; cap].into_boxed_slice(),
            head: 0,
            len: 0,
        }
    }

    fn capacity(&self) -> usize {
        self.buf.len()
    }

    /// Append `data`, discarding oldest bytes if needed. Returns bytes dropped.
    fn push(&mut self, data: &[u8]) -> usize {
        let cap = self.capacity();
        let mut dropped = 0;
        // A write larger than the ring can only keep its tail. Those bytes are
        // just as lost as the ones evicted below, so they count as drops too.
        let data = if data.len() > cap {
            dropped += data.len() - cap;
            &data[data.len() - cap..]
        } else {
            data
        };
        let mut overflow = (self.len + data.len()).saturating_sub(cap);
        if overflow > 0 {
            // Evict whole records. Dropping an arbitrary byte count
            // shifts every following packet out of phase, and a TS reader then
            // has to resynchronise on 0x47 — measured over WiFi as a stream
            // that parses for 750 kB and turns to noise after the first drop.
            // Rounding up keeps the ring's contents packet-aligned as long as
            // the producer feeds it whole packets, which the 4K tap does.
            let aligned = overflow.div_ceil(self.evict_unit) * self.evict_unit;
            overflow = aligned.min(self.len);
            self.head = (self.head + overflow) % cap;
            self.len -= overflow;
            dropped += overflow;
        }
        let tail = (self.head + self.len) % cap;
        let first = (cap - tail).min(data.len());
        self.buf[tail..tail + first].copy_from_slice(&data[..first]);
        if first < data.len() {
            let rest = data.len() - first;
            self.buf[..rest].copy_from_slice(&data[first..]);
        }
        self.len += data.len();
        dropped
    }

    fn pop(&mut self, out: &mut [u8]) -> usize {
        let cap = self.capacity();
        let n = self.len.min(out.len());
        let first = (cap - self.head).min(n);
        out[..first].copy_from_slice(&self.buf[self.head..self.head + first]);
        if first < n {
            out[first..n].copy_from_slice(&self.buf[..n - first]);
        }
        self.head = (self.head + n) % cap;
        self.len -= n;
        n
    }
}

/// A subscriber's buffers, under one lock.
struct Inbox {
    ring: ByteRing,
    /// Holds pre-sync bytes. A source may hand over one packet at a time, so
    /// the boundary search has to span chunks; empty once synced.
    pending: Vec<u8>,
    synced: bool,
}

/// One HTTP client's view of the stream.
pub struct Subscriber {
    pub id: String,
    pub priority: i32,
    pub agent: String,
    pub url: String,
    inbox: Mutex<Inbox>,
    ready: Condvar,
    closed: AtomicBool,
    bytes: AtomicU64,
    dropped: AtomicU64,
    skipped: AtomicU64,
}

impl Subscriber {
    fn new(id: String, priority: i32, agent: String, url: String, cap: usize) -> Self {
        Subscriber {
            id,
            priority,
            agent,
            url,
            inbox: Mutex::new(Inbox {
                ring: ByteRing::with_evict_unit(cap.max(MIN_RING), TS_PACKET),
                pending: Vec::new(),
                synced: false,
            }),
            ready: Condvar::new(),
            closed: AtomicBool::new(false),
            bytes: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
        }
    }

    /// Hand `data` to this client.
    ///
    /// A client joining a session already in flight starts mid-packet, so
    /// until a packet boundary is found the bytes are held back rather than
    /// forwarded: the body should be a sequence of TS packets, not a byte
    /// stream the client has to resynchronise itself.
    fn offer(&self, data: &[u8]) {
        let mut inbox = self.inbox.lock().unwrap_or_else(|e| e.into_inner());

        let dropped = if inbox.synced {
            let d = inbox.ring.push(data);
            self.bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
            d
        } else {
            if RAW_PASSTHROUGH.load(Ordering::Relaxed) {
                inbox.synced = true;
                let d = inbox.ring.push(data);
                self.bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
                d
            } else {
            inbox.pending.extend_from_slice(data);
            match ts_alignment(&inbox.pending) {
                Some(off) => {
                    inbox.synced = true;
                    self.skipped.fetch_add(off as u64, Ordering::Relaxed);
                    let aligned = std::mem::take(&mut inbox.pending);
                    let payload = &aligned[off..];
                    let d = inbox.ring.push(payload);
                    self.bytes.fetch_add(payload.len() as u64, Ordering::Relaxed);
                    d
                }
                None if inbox.pending.len() as u64 >= RESYNC_GIVE_UP => {
                    // Never staying silent is the point: a source that does not
                    // look like TS is a finding, not a reason to serve nothing.
                    inbox.synced = true;
                    let raw = std::mem::take(&mut inbox.pending);
                    eprintln!(
                        "mkra: no TS packet boundary in the first {} bytes for {}; \
                         passing bytes through unaligned",
                        raw.len(),
                        self.id
                    );
                    let d = inbox.ring.push(&raw);
                    self.bytes.fetch_add(raw.len() as u64, Ordering::Relaxed);
                    d
                }
                None => return,
            }
            }
        };

        drop(inbox);
        if dropped > 0 {
            self.dropped.fetch_add(dropped as u64, Ordering::Relaxed);
        }
        self.ready.notify_one();
    }

    /// Block until bytes are available, the subscriber is closed, or `timeout`.
    pub fn read(&self, out: &mut [u8], timeout: Duration) -> usize {
        let mut inbox = self.inbox.lock().unwrap_or_else(|e| e.into_inner());
        if inbox.ring.len == 0 && !self.closed.load(Ordering::Relaxed) {
            let (g, _) = self
                .ready
                .wait_timeout(inbox, timeout)
                .unwrap_or_else(|e| e.into_inner());
            inbox = g;
        }
        inbox.ring.pop(out)
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        self.ready.notify_all();
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    /// `(packets, dropped_packets)` in Mirakurun's units.
    pub fn counters(&self) -> (u64, u64) {
        (
            self.bytes.load(Ordering::Relaxed) / TS_PACKET as u64,
            self.dropped.load(Ordering::Relaxed) / TS_PACKET as u64,
        )
    }
}

/// A live pump feeding every subscriber on one channel.
pub struct Session {
    pub channel_key: String,
    pub source_desc: String,
    pub started: Instant,
    subscribers: Mutex<Vec<Arc<Subscriber>>>,
    stop: AtomicBool,
    pub bytes_in: AtomicU64,
    pub errors: AtomicU64,
}

impl Session {
    pub fn subscribers(&self) -> Vec<Arc<Subscriber>> {
        self.subscribers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        for s in self.subscribers() {
            s.close();
        }
    }
}

/// What a subscribe attempt can fail with.
#[derive(Debug)]
pub enum SubscribeError {
    /// Another channel is already being served.
    Busy(String),
    /// The TS backend could not be opened.
    Source(io::Error),
    /// Too many clients.
    TooManyClients,
}

pub struct BrokerConfig {
    pub ring_bytes: usize,
    pub max_clients: usize,
    /// How long the pump keeps running after the last client leaves, so a
    /// client that reconnects immediately does not force a re-tune.
    pub linger: Duration,
}

impl Default for BrokerConfig {
    fn default() -> Self {
        BrokerConfig {
            ring_bytes: 1024 * 1024, // ~250 ms at the 32 Mbit/s advanced-BS rate
            max_clients: 4,
            linger: Duration::from_secs(3),
        }
    }
}

/// Builds a fresh `TsSource` for a channel. Returning a boxed trait object
/// keeps the driver and the file backends interchangeable.
pub type SourceFactory =
    Box<dyn Fn(&str) -> Result<Box<dyn TsSource>, io::Error> + Send + Sync>;

pub struct Broker {
    cfg: BrokerConfig,
    factory: SourceFactory,
    state: Mutex<Option<Arc<Session>>>,
}

impl Broker {
    pub fn new(cfg: BrokerConfig, factory: SourceFactory) -> Arc<Self> {
        Arc::new(Broker {
            cfg,
            factory,
            state: Mutex::new(None),
        })
    }

    pub fn current(&self) -> Option<Arc<Session>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn subscribe(
        self: &Arc<Self>,
        channel_key: &str,
        id: String,
        priority: i32,
        agent: String,
        url: String,
    ) -> Result<(Arc<Session>, Arc<Subscriber>), SubscribeError> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());

        if let Some(session) = state.as_ref() {
            if session.channel_key != channel_key {
                return Err(SubscribeError::Busy(session.channel_key.clone()));
            }
            let session = Arc::clone(session);
            let mut subs = session.subscribers.lock().unwrap_or_else(|e| e.into_inner());
            if subs.len() >= self.cfg.max_clients {
                return Err(SubscribeError::TooManyClients);
            }
            let sub = Arc::new(Subscriber::new(id, priority, agent, url, self.cfg.ring_bytes));
            subs.push(Arc::clone(&sub));
            drop(subs);
            return Ok((session, sub));
        }

        let source = (self.factory)(channel_key).map_err(SubscribeError::Source)?;
        let sub = Arc::new(Subscriber::new(id, priority, agent, url, self.cfg.ring_bytes));
        let session = Arc::new(Session {
            channel_key: channel_key.to_string(),
            source_desc: source.describe(),
            started: Instant::now(),
            subscribers: Mutex::new(vec![Arc::clone(&sub)]),
            stop: AtomicBool::new(false),
            bytes_in: AtomicU64::new(0),
            errors: AtomicU64::new(0),
        });
        *state = Some(Arc::clone(&session));
        drop(state);

        let broker = Arc::clone(self);
        let pump_session = Arc::clone(&session);
        let linger = self.cfg.linger;
        std::thread::Builder::new()
            .name("mkra-pump".into())
            .stack_size(128 * 1024)
            .spawn(move || pump(broker, pump_session, source, linger))
            .map_err(SubscribeError::Source)?;

        Ok((session, sub))
    }

    pub fn unsubscribe(&self, session: &Arc<Session>, sub: &Arc<Subscriber>) {
        sub.close();
        // Report what the client actually got. Drops here mean the client read
        // slower than the tuner produced, which is exactly what a viewer sees
        // as picture break-up — and it is invisible in the tap's own counters,
        // because the tap was keeping up fine.
        let (packets, drops) = sub.counters();
        eprintln!(
            "mkra: client {} left {}: {packets} packets, {drops} dropped ({:.3}%)",
            sub.id,
            session.channel_key,
            if packets + drops == 0 {
                0.0
            } else {
                100.0 * drops as f64 / (packets + drops) as f64
            }
        );
        let mut subs = session.subscribers.lock().unwrap_or_else(|e| e.into_inner());
        subs.retain(|s| !Arc::ptr_eq(s, sub));
    }

    /// Stop the current session outright (used on shutdown).
    pub fn shutdown(&self) {
        if let Some(s) = self.current() {
            s.stop();
        }
    }
}

/// The single reader thread. Owns the `TsSource` for the session's lifetime.
fn pump(
    broker: Arc<Broker>,
    session: Arc<Session>,
    mut source: Box<dyn TsSource>,
    linger: Duration,
) {
    let mut scratch: Vec<u8> = Vec::with_capacity(256 * 1024);
    let mut idle_since: Option<Instant> = None;
    let mut consecutive_errors = 0u32;

    loop {
        if session.stop.load(Ordering::Relaxed) {
            break;
        }

        let subs = session.subscribers();
        if subs.is_empty() {
            let since = *idle_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= linger {
                break;
            }
        } else {
            idle_since = None;
        }

        scratch.clear();
        match source.read_into(&mut scratch) {
            Ok(0) => {
                // Nothing available. `read_into` already blocked or slept, so
                // spinning here is bounded by the source's own timeout.
                consecutive_errors = 0;
            }
            Ok(n) => {
                consecutive_errors = 0;
                session.bytes_in.fetch_add(n as u64, Ordering::Relaxed);
                for s in &subs {
                    if !s.is_closed() {
                        s.offer(&scratch);
                    }
                }
            }
            Err(e) => {
                session.errors.fetch_add(1, Ordering::Relaxed);
                consecutive_errors += 1;
                eprintln!(
                    "mkra: read error on {} ({consecutive_errors}): {e}",
                    session.channel_key
                );
                // A handful of transient errors is tolerable; a persistent one
                // means the buffer went away and the session cannot recover.
                if consecutive_errors >= 20 || e.kind() == io::ErrorKind::UnexpectedEof {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }

    session.stop();
    drop(source); // destroys the reader port before the slot is released

    let mut state = broker.state.lock().unwrap_or_else(|e| e.into_inner());
    if state.as_ref().is_some_and(|s| Arc::ptr_eq(s, &session)) {
        *state = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_round_trips_within_capacity() {
        let mut r = ByteRing::new(1024);
        assert_eq!(r.push(b"hello world"), 0);
        let mut out = [0u8; 32];
        let n = r.pop(&mut out);
        assert_eq!(&out[..n], b"hello world");
        assert_eq!(r.pop(&mut out), 0);
    }

    #[test]
    fn ring_wraps_correctly() {
        let mut r = ByteRing::new(8);
        r.push(b"abcdef");
        let mut out = [0u8; 4];
        assert_eq!(r.pop(&mut out), 4);
        assert_eq!(&out, b"abcd");
        // Now write across the physical end of the buffer.
        assert_eq!(r.push(b"ghij"), 0);
        let mut all = [0u8; 8];
        let n = r.pop(&mut all);
        assert_eq!(&all[..n], b"efghij");
    }

    #[test]
    fn ring_drops_oldest_on_overflow() {
        let mut r = ByteRing::new(8);
        r.push(b"12345678");
        // Four more bytes must evict the four oldest.
        assert_eq!(r.push(b"abcd"), 4);
        let mut out = [0u8; 8];
        let n = r.pop(&mut out);
        assert_eq!(&out[..n], b"5678abcd");
    }

    #[test]
    fn packet_aligned_ring_never_leaves_a_partial_packet() {
        // Four packets of capacity, filled, then one more byte arrives: a
        // byte-granular ring would evict 1 byte and put every later packet out
        // of phase. This must evict a whole packet instead.
        let cap = 4 * TS_PACKET;
        let mut r = ByteRing::with_evict_unit(cap, TS_PACKET);
        let mut packets = Vec::new();
        for i in 0..4u8 {
            let mut p = [0u8; TS_PACKET];
            p[0] = 0x47;
            p[1] = i;
            packets.extend_from_slice(&p);
        }
        assert_eq!(r.push(&packets), 0);

        let mut fifth = [0u8; TS_PACKET];
        fifth[0] = 0x47;
        fifth[1] = 4;
        assert_eq!(r.push(&fifth), TS_PACKET);

        let mut out = vec![0u8; cap];
        let n = r.pop(&mut out);
        assert_eq!(n % TS_PACKET, 0);
        for (i, packet) in out[..n].chunks_exact(TS_PACKET).enumerate() {
            assert_eq!(packet[0], 0x47, "packet {i} lost sync");
            assert_eq!(packet[1], i as u8 + 1, "oldest packet should be gone");
        }
    }

    #[test]
    fn ring_write_larger_than_capacity_keeps_the_tail() {
        let mut r = ByteRing::new(4);
        let dropped = r.push(b"abcdefgh");
        let mut out = [0u8; 4];
        let n = r.pop(&mut out);
        // Keeping the tail is what a live stream wants: stay current.
        assert_eq!(&out[..n], b"efgh");
        // Every byte that did not survive must be counted, including the four
        // discarded by the over-capacity truncation itself.
        assert_eq!(dropped, 4);
    }

    fn ts_bytes(packets: usize) -> Vec<u8> {
        let mut v = Vec::new();
        for i in 0..packets {
            let mut p = [0u8; TS_PACKET];
            p[0] = 0x47;
            p[2] = i as u8;
            v.extend_from_slice(&p);
        }
        v
    }

    #[test]
    fn alignment_finds_the_packet_boundary() {
        let ts = ts_bytes(10);
        assert_eq!(ts_alignment(&ts), Some(0));

        // Joining mid-packet: the boundary is where the next packet starts.
        assert_eq!(ts_alignment(&ts[100..]), Some(TS_PACKET - 100));

        // A lone 0x47 inside payload must not be mistaken for a boundary.
        let mut noise = vec![0u8; 400];
        noise[5] = 0x47;
        assert_eq!(ts_alignment(&noise), None);
    }

    #[test]
    fn late_joiner_is_delivered_packet_aligned_data() {
        let s = Subscriber::new("t".into(), 0, String::new(), String::new(), MIN_RING * 4);
        let ts = ts_bytes(40);
        // Hand it a stream that starts 100 bytes into a packet.
        s.offer(&ts[100..]);
        let mut out = vec![0u8; MIN_RING * 4];
        let n = s.read(&mut out, Duration::from_millis(10));
        assert!(n > 0);
        assert_eq!(out[0], 0x47, "stream must start on a sync byte");
        assert_eq!(out[TS_PACKET], 0x47);
        // The 88 bytes of partial packet were skipped, not delivered.
        assert_eq!(n % TS_PACKET, 0);
    }

    #[test]
    fn already_aligned_data_is_passed_through_untouched() {
        let s = Subscriber::new("t".into(), 0, String::new(), String::new(), MIN_RING * 4);
        let ts = ts_bytes(40);
        s.offer(&ts);
        let mut out = vec![0u8; MIN_RING * 4];
        let n = s.read(&mut out, Duration::from_millis(10));
        assert_eq!(n, ts.len());
        assert_eq!(&out[..n], &ts[..]);
    }

    #[test]
    fn subscriber_ring_is_never_smaller_than_the_floor() {
        // A caller asking for a 1-byte ring must not get one packet of
        // capacity; the floor is applied where the config enters the system.
        let s = Subscriber::new("t".into(), 0, String::new(), String::new(), 1);
        let ts = ts_bytes(MIN_RING / TS_PACKET);
        s.offer(&ts);
        let mut out = vec![0u8; MIN_RING * 2];
        assert_eq!(s.read(&mut out, Duration::from_millis(10)), ts.len());
        assert_eq!(s.counters().1, 0, "nothing should have been dropped");
    }

    struct Counting(u32);
    impl TsSource for Counting {
        fn read_into(&mut self, out: &mut Vec<u8>) -> io::Result<usize> {
            if self.0 == 0 {
                std::thread::sleep(Duration::from_millis(5));
                return Ok(0);
            }
            self.0 -= 1;
            let mut p = [0u8; TS_PACKET];
            p[0] = 0x47;
            out.extend_from_slice(&p);
            Ok(TS_PACKET)
        }
        fn describe(&self) -> String {
            "counting".into()
        }
    }

    fn test_broker() -> Arc<Broker> {
        Broker::new(
            BrokerConfig {
                ring_bytes: 4096,
                max_clients: 2,
                linger: Duration::from_millis(50),
            },
            Box::new(|_| Ok(Box::new(Counting(100)) as Box<dyn TsSource>)),
        )
    }

    #[test]
    fn subscriber_receives_pumped_data() {
        let b = test_broker();
        let (session, sub) = b
            .subscribe("BS17_0", "t".into(), 0, "test".into(), "/".into())
            .unwrap();
        let mut got = 0;
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut buf = [0u8; 4096];
        while got < TS_PACKET * 10 && Instant::now() < deadline {
            got += sub.read(&mut buf, Duration::from_millis(50));
        }
        assert!(got >= TS_PACKET * 10, "only got {got} bytes");
        b.unsubscribe(&session, &sub);
    }

    #[test]
    fn second_channel_is_rejected_while_one_is_live() {
        let b = test_broker();
        let (session, sub) = b
            .subscribe("BS17_0", "a".into(), 0, "test".into(), "/".into())
            .unwrap();
        match b.subscribe("BS7_0", "b".into(), 0, "test".into(), "/".into()) {
            Err(SubscribeError::Busy(ch)) => assert_eq!(ch, "BS17_0"),
            Err(e) => panic!("expected Busy, got {e:?}"),
            Ok(_) => panic!("expected Busy, but a second channel was accepted"),
        }
        // The same channel joins the existing session instead.
        let (s2, sub2) = b
            .subscribe("BS17_0", "c".into(), 0, "test".into(), "/".into())
            .unwrap();
        assert!(Arc::ptr_eq(&session, &s2));
        b.unsubscribe(&session, &sub);
        b.unsubscribe(&s2, &sub2);
    }

    #[test]
    fn client_limit_is_enforced() {
        let b = test_broker();
        let (s, a) = b
            .subscribe("BS17_0", "a".into(), 0, "".into(), "/".into())
            .unwrap();
        let (_, bb) = b
            .subscribe("BS17_0", "b".into(), 0, "".into(), "/".into())
            .unwrap();
        assert!(matches!(
            b.subscribe("BS17_0", "c".into(), 0, "".into(), "/".into()),
            Err(SubscribeError::TooManyClients)
        ));
        b.unsubscribe(&s, &a);
        b.unsubscribe(&s, &bb);
    }

    #[test]
    fn session_ends_after_the_last_client_leaves() {
        let b = test_broker();
        let (session, sub) = b
            .subscribe("BS17_0", "a".into(), 0, "".into(), "/".into())
            .unwrap();
        b.unsubscribe(&session, &sub);
        let deadline = Instant::now() + Duration::from_secs(2);
        while b.current().is_some() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(b.current().is_none(), "pump did not release the session");
    }
}
