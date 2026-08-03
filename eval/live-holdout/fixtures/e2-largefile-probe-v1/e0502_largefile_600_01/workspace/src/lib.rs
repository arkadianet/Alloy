//! Instrumentation toolkit: sensor channels, ledgers, logs, readers, and the
//! report pipeline that ties them together.
//!
//! Every module stands alone; `pipeline` composes several of them into an
//! end-to-end run report.

pub mod byte_reader {
    /// A forward-only cursor over a borrowed byte buffer.
    ///
    /// A `Reader` never owns its bytes: it borrows the caller's buffer and only
    /// tracks how much of it has been consumed.
    pub struct Reader<'a> {
        bytes: &'a [u8],
        pos: usize,
    }

    impl<'a> Reader<'a> {
        /// Creates a reader positioned at the start of `bytes`.
        pub fn new(bytes: &'a [u8]) -> Reader<'a> {
            Reader { bytes, pos: 0 }
        }

        /// The bytes that have not been consumed yet, still borrowed from the
        /// caller's buffer.
        pub fn remaining(&self) -> &'a [u8] {
            &self.bytes[self.pos..]
        }

        /// Consumes `n` further bytes.
        ///
        /// Callers must not advance past the end of the buffer.
        pub fn advance(&mut self, n: usize) {
            self.pos += n;
        }

        /// How many bytes have been consumed so far.
        pub fn position(&self) -> usize {
            self.pos
        }

        /// True once every byte has been consumed.
        pub fn is_done(&self) -> bool {
            self.pos == self.bytes.len()
        }
    }
}

pub mod name_order {
    /// Returns whichever of the two names sorts first, borrowed from the argument
    /// it came from.
    ///
    /// Ties are broken in favour of `left`.
    pub fn first_alphabetically<'a>(left: &'a str, right: &'a str) -> &'a str {
        if right < left {
            right
        } else {
            left
        }
    }
}

pub mod reading_series {
    /// A borrowed run of sensor readings together with the factor they scale by.
    ///
    /// A `Series` never owns its readings: it borrows the caller's slice for as
    /// long as the series is alive.
    pub struct Series<'a> {
        readings: &'a [i32],
        scale: i32,
    }

    /// Builds a series that borrows `readings` and scales each one by `scale`.
    pub fn series(readings: &[i32], scale: i32) -> Series<'_> {
        Series { readings, scale }
    }

    /// The borrowed readings, exactly as they were supplied.
    pub fn readings<'a>(series: &Series<'a>) -> &'a [i32] {
        series.readings
    }

    /// Sum of every reading multiplied by the series scale.
    pub fn scaled_total(series: &Series<'_>) -> i32 {
        series.readings.iter().map(|r| r * series.scale).sum()
    }

    /// How many readings the series covers.
    pub fn count(series: &Series<'_>) -> usize {
        series.readings.len()
    }
}

pub mod config_parse {
    use std::collections::BTreeMap;

    /// Parses `config` into a map from setting name to setting value.
    ///
    /// Each line is `key=value`, split at its first `=`; everything before that
    /// `=` is the key and everything after it is the value, both trimmed of
    /// surrounding whitespace. Lines with no `=` are skipped. When a key is given
    /// more than once, the last line wins.
    pub fn parse_config(config: &str) -> BTreeMap<String, String> {
        config
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
            .collect()
    }
}

pub mod labeled_render {
    use std::fmt::Display;

    /// Renders every item of `items` as one `label[index] = item` line.
    ///
    /// Lines are emitted in slice order, indices start at zero, and each line —
    /// including the last — ends with a newline. Items are shown the way a user
    /// would read them, not as a debug dump.
    pub fn render_labeled<T: Display>(label: &str, items: &[T]) -> String {
        let mut out = String::new();
        for (index, item) in items.iter().enumerate() {
            out.push_str(&format!("{label}[{index}] = {item}\n"));
        }
        out
    }
}

pub mod allow_filter {
    /// Returns the items that do not appear anywhere in `blocked`.
    ///
    /// The surviving items keep their original relative order, and repeats are
    /// kept: this filters, it never sorts and never de-duplicates.
    pub fn retain_allowed<T: Clone + PartialEq>(items: &[T], blocked: &[T]) -> Vec<T> {
        let mut allowed = Vec::new();
        for item in items {
            if !blocked.contains(item) {
                allowed.push(item.clone());
            }
        }
        allowed
    }
}

pub mod frame_totals {
    /// Sums the byte lengths of every frame in `frame_lengths`.
    ///
    /// The running total is accumulated and reported as a 64-bit count, so that a
    /// long run of maximum-size frames can never overflow the reported value.
    pub fn total_bytes(frame_lengths: &[u32]) -> u64 {
        let mut total: u64 = 0;
        for &len in frame_lengths {
            total += u64::from(len);
        }
        total
    }
}

pub mod keyed_lookup {
    /// Error reported when a lookup finds no matching entry.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum LookupError {
        MissingKey,
    }

    /// Reports the value stored beside the first entry whose key equals `key`.
    ///
    /// Returns `Err(LookupError::MissingKey)` when no entry matches.
    pub fn lookup(entries: &[(&str, i32)], key: &str) -> Result<i32, LookupError> {
        entries
            .iter()
            .find(|(entry_key, _)| *entry_key == key)
            .map(|(_, value)| *value)
            .ok_or(LookupError::MissingKey)
    }
}

pub mod slug {
    /// Builds a slug for `title`: ASCII letters and digits are kept in lower case,
    /// and every run of other characters becomes a single `-` separator.
    ///
    /// The slug is freshly built rather than borrowed out of `title`, and it never
    /// starts or ends with a separator.
    pub fn slugify(title: &str) -> String {
        let mut slug = String::with_capacity(title.len());
        let mut separator_pending = false;
        for ch in title.chars() {
            if ch.is_ascii_alphanumeric() {
                if separator_pending && !slug.is_empty() {
                    slug.push('-');
                }
                separator_pending = false;
                slug.push(ch.to_ascii_lowercase());
            } else {
                separator_pending = true;
            }
        }
        slug
    }
}

pub mod upload_store {
    /// One file staged for upload.
    pub struct Upload {
        /// Display name of the file.
        pub name: String,
        /// Raw payload bytes.
        pub bytes: Vec<u8>,
    }

    impl Upload {
        /// Consumes the upload and hands back its payload unchanged.
        pub fn into_bytes(self) -> Vec<u8> {
            self.bytes
        }
    }

    /// Stores `upload` and returns a receipt of the form `<name>: <n> bytes`,
    /// where `<n>` is the length of the stored payload.
    pub fn store(upload: Upload) -> String {
        let Upload { name, bytes } = upload;
        format!("{}: {} bytes", name, bytes.len())
    }
}

pub mod line_labels {
    /// Labels every entry by putting `prefix` in front of it, returning one
    /// `<prefix><entry>` string per entry, in the order the entries were given.
    pub fn label_all(prefix: String, entries: &[&str]) -> Vec<String> {
        let mut out = Vec::new();
        for entry in entries {
            let mut line = String::with_capacity(prefix.len() + entry.len());
            line.push_str(&prefix);
            line.push_str(entry);
            out.push(line);
        }
        out
    }
}

pub mod command_describe {
    /// A parsed command line.
    pub struct Command {
        /// The command name, without arguments.
        pub name: String,
        /// The arguments that followed the name, in order.
        pub args: Vec<String>,
    }

    /// Consumes `command` and returns its name together with a summary of the
    /// form `<name> takes <n> arg(s)`, where `<n>` is the number of arguments.
    pub fn describe(command: Command) -> (String, String) {
        let summary = format!("{} takes {} arg(s)", command.name, command.args.len());
        let name = command.name;
        (name, summary)
    }
}

pub mod ledger_audit {
    /// A set of counters plus an audit log of the adjustments applied to them.
    pub struct Ledger {
        counters: Vec<i64>,
        audit: Vec<i64>,
    }

    impl Ledger {
        /// Builds a ledger over `counters` with an empty audit log.
        pub fn new(counters: Vec<i64>) -> Self {
            Self {
                counters,
                audit: Vec::new(),
            }
        }

        /// Adds `delta` to the counter at `index` and records `delta` in the audit
        /// log.
        ///
        /// Callers must pass an index that is in range.
        pub fn adjust(&mut self, index: usize, delta: i64) {
            // `counters` and `audit` are disjoint fields, so borrowing them
            // separately keeps both live at once; going through `push_audit` would
            // instead reborrow all of `*self`.
            let counter = &mut self.counters[index];
            self.audit.push(delta);
            *counter += delta;
        }

        /// Appends `delta` to the audit log without touching any counter.
        pub fn push_audit(&mut self, delta: i64) {
            self.audit.push(delta);
        }

        /// The current counter values.
        pub fn counters(&self) -> &[i64] {
            &self.counters
        }

        /// The adjustments recorded so far, oldest first.
        pub fn audit(&self) -> &[i64] {
            &self.audit
        }
    }
}

pub mod series_extend {
    /// Appends one new reading to `series`.
    ///
    /// The newest reading is first clamped down to `cap` when it exceeds `cap`,
    /// and the appended reading is that (possibly clamped) newest reading plus
    /// `delta`.
    ///
    /// Callers must pass a non-empty `series`.
    pub fn extend_clamped(series: &mut Vec<i64>, cap: i64, delta: i64) {
        // The borrow of the newest element ends with this block, so the push below
        // is free to borrow `series` again.
        let next = {
            let newest = series.last_mut().expect("series must not be empty");
            if *newest > cap {
                *newest = cap;
            }
            *newest + delta
        };
        series.push(next);
    }
}

pub mod balance_transfer {
    /// Moves `amount` from the balance at `from` to the balance at `to`.
    ///
    /// Callers must pass two distinct indices that are both in range.
    pub fn transfer(balances: &mut [i64], from: usize, to: usize, amount: i64) {
        balances[from] -= amount;
        balances[to] += amount;
    }
}

pub mod channel_updates {
    //! Sensor channels that record a reading and report both sides of the write.

    /// What a channel looked like immediately before and after a reading landed.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Update {
        /// The channel's value before the reading was written.
        pub previous: i32,
        /// The channel's value after the reading was written.
        pub current: i32,
    }

    impl Update {
        /// How far the channel moved: `current - previous`.
        pub fn delta(&self) -> i32 {
            self.current - self.previous
        }
    }

    /// Writes `reading` into `channel`, capping the stored value at `ceiling`, and
    /// reports the channel's value on both sides of that write.
    ///
    /// Callers must pass a channel index that is in range.
    pub fn record(channels: &mut [i32], channel: usize, reading: i32, ceiling: i32) -> Update {
        let previous = &channels[channel];
        write_capped(channels, channel, reading, ceiling);
        Update {
            previous: *previous,
            current: channels[channel],
        }
    }

    /// Stores the capped reading in place.
    fn write_capped(channels: &mut [i32], channel: usize, reading: i32, ceiling: i32) {
        channels[channel] = reading.min(ceiling);
    }
}

pub mod rate_counters {
    //! Rate counters that saturate at a caller-supplied ceiling.

    /// Adds `delta` to the counter at `index`, clamping the stored result at
    /// `limit`, and returns the value that is now stored in the counter.
    ///
    /// Because the clamp is applied before the value is reported, the returned
    /// value is never greater than `limit`.
    ///
    /// Callers must pass an index that is in range, and a `delta` whose sum with
    /// the existing counter does not overflow.
    pub fn bump(counters: &mut [i64], index: usize, delta: i64, limit: i64) -> i64 {
        clamp_add(counters, index, delta, limit);
        counters[index]
    }

    /// Performs the clamped accumulation in place.
    fn clamp_add(counters: &mut [i64], index: usize, delta: i64, limit: i64) {
        counters[index] = (counters[index] + delta).min(limit);
    }
}

pub mod account_ledger {
    //! A tiny account ledger held in a caller-owned slice of balances.

    /// Adds `amount` to the account at `index` and returns the *opening* balance:
    /// the balance the account held before this deposit was applied.
    ///
    /// Callers must pass an index that is in range, and an `amount` whose sum with
    /// the existing balance does not overflow.
    pub fn deposit(balances: &mut [i64], index: usize, amount: i64) -> i64 {
        let opening = balances[index];
        credit(balances, index, amount);
        opening
    }

    /// Applies a credit in place. Kept separate so the deposit path reads as
    /// "observe, then mutate".
    fn credit(balances: &mut [i64], index: usize, amount: i64) {
        balances[index] += amount;
    }
}

pub mod log_flush {
    /// Flushes the pending log: returns everything the log held with `terminator`
    /// appended, and leaves the log empty.
    ///
    /// An empty log and an empty terminator are both ordinary inputs.
    pub fn flush(log: &mut String, terminator: &str) -> String {
        let pending = log.clone();
        log.clear();
        let mut flushed = pending.to_string();
        flushed.push_str(terminator);
        flushed
    }
}

pub mod sensor_history {
    /// A single labelled sensor reading.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Reading {
        pub label: String,
        pub value: i64,
    }

    /// A sensor that keeps every reading it is given, oldest first.
    #[derive(Debug, Default)]
    pub struct Sensor {
        history: Vec<Reading>,
    }

    impl Sensor {
        /// A sensor with no readings yet.
        pub fn new() -> Self {
            Sensor {
                history: Vec::new(),
            }
        }

        /// Records `reading` and returns the reading that was the latest one
        /// before this call, or `None` when this is the first reading.
        pub fn record(&mut self, reading: Reading) -> Option<Reading> {
            let previous = self.history.last().cloned();
            self.history.push(reading);
            previous
        }

        /// Every reading recorded so far, oldest first.
        pub fn history(&self) -> &[Reading] {
            &self.history
        }
    }
}

pub mod task_queue {
    /// Appends every task in `incoming` to the end of `queue` and returns the
    /// tasks the queue held before the append, in their original order.
    ///
    /// An empty queue and an empty `incoming` are both ordinary inputs.
    pub fn absorb(queue: &mut Vec<String>, incoming: &[String]) -> Vec<String> {
        let previous = queue.clone();
        queue.extend_from_slice(incoming);
        previous
    }
}

pub mod label_suffix {
    /// Appends `suffix` to every label in `labels`, in place.
    ///
    /// The labels keep their order and the slice keeps its length; only the
    /// contents of each label grow.
    pub fn append_suffix(labels: &mut [String], suffix: &str) {
        for label in labels.iter_mut() {
            label.push_str(suffix);
        }
    }
}

pub mod running_totals {
    /// Returns the running totals of `values`.
    ///
    /// Element `i` of the result is the sum of `values[..=i]`, so the result
    /// always has the same length as `values`.
    pub fn running_totals(values: &[i64]) -> Vec<i64> {
        let mut totals = Vec::new();
        let mut sum: i64 = 0;
        for value in values {
            sum += value;
            totals.push(sum);
        }
        totals
    }
}

pub mod event_log {
    /// An append-only log of recorded events.
    #[derive(Debug, Default)]
    pub struct EventLog {
        entries: Vec<String>,
    }

    impl EventLog {
        /// Creates a log with no entries.
        pub fn new() -> Self {
            Self {
                entries: Vec::new(),
            }
        }

        /// Appends `entry` to the end of the log.
        pub fn record(&mut self, entry: &str) {
            self.entries.push(entry.to_string());
        }

        /// Returns the number of recorded entries.
        pub fn len(&self) -> usize {
            self.entries.len()
        }

        /// Returns true when nothing has been recorded yet.
        pub fn is_empty(&self) -> bool {
            self.entries.is_empty()
        }

        /// Returns the entries in the order they were recorded.
        pub fn entries(&self) -> &[String] {
            &self.entries
        }
    }
}

pub mod pipeline {
    //! End-to-end report assembly built from the crate's other modules.

    use super::channel_updates::{record, Update};
    use super::config_parse::parse_config;
    use super::frame_totals::total_bytes;
    use super::labeled_render::render_labeled;
    use super::slug::slugify;

    /// One run's report: the slug naming the run, the rendered channel lines,
    /// and the total payload bytes the run carried.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct RunReport {
        pub slug: String,
        pub rendered: String,
        pub payload_bytes: u64,
    }

    /// Applies every `(channel, reading)` sample to `channels`, capping each
    /// stored value at `ceiling`, and returns the per-sample updates in order.
    pub fn apply_samples(
        channels: &mut [i32],
        samples: &[(usize, i32)],
        ceiling: i32,
    ) -> Vec<Update> {
        samples
            .iter()
            .map(|&(channel, reading)| record(channels, channel, reading, ceiling))
            .collect()
    }

    /// Builds the report for one run.
    ///
    /// `config` names the run under its `title` key; a missing title falls
    /// back to `untitled`. The rendered section lists the channel values
    /// under the slug as their label.
    pub fn run_report(config: &str, channels: &[i32], frame_lengths: &[u32]) -> RunReport {
        let settings = parse_config(config);
        let title = settings
            .get("title")
            .map(String::as_str)
            .unwrap_or("untitled");
        let slug = slugify(title);
        let rendered = render_labeled(&slug, channels);
        RunReport {
            slug,
            rendered,
            payload_bytes: total_bytes(frame_lengths),
        }
    }
}
