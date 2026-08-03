//! Hidden integrity oracle for `e0502_largefile_600_01`.
//!
//! The planted bug lives in `channel_updates` and has its own oracle in
//! `semantic.rs`. Every test here guards a module the repair has no reason to
//! touch, because the probe's question is collateral damage: a whole-file
//! regeneration that fixes the planted bug but drops or rewrites an unrelated
//! function must fail — at compile time when a symbol disappears or a
//! signature drifts, at runtime when behavior drifts.

use e0502_largefile_600_01 as probe;

#[test]
fn byte_reader_tracks_position_and_remaining() {
    let bytes = [1u8, 2, 3, 4, 5];
    let mut reader = probe::byte_reader::Reader::new(&bytes);
    assert_eq!(reader.position(), 0);
    assert!(!reader.is_done());
    reader.advance(2);
    assert_eq!(reader.remaining(), &[3, 4, 5]);
    assert_eq!(reader.position(), 2);
    reader.advance(3);
    assert!(reader.is_done());
    assert_eq!(reader.remaining(), &[] as &[u8]);
}

#[test]
fn byte_reader_remaining_borrows_the_callers_buffer() {
    // `remaining` must return bytes borrowed from the caller's buffer, so the
    // slice must be usable after the reader is gone.
    let bytes = [9u8, 8, 7];
    let rest = {
        let mut reader = probe::byte_reader::Reader::new(&bytes);
        reader.advance(1);
        reader.remaining()
    };
    assert_eq!(rest, &[8, 7]);
}

#[test]
fn name_order_returns_the_alphabetically_first_name() {
    assert_eq!(
        probe::name_order::first_alphabetically("pine", "aspen"),
        "aspen"
    );
    assert_eq!(
        probe::name_order::first_alphabetically("aspen", "pine"),
        "aspen"
    );
    assert_eq!(probe::name_order::first_alphabetically("elm", "elm"), "elm");
}

#[test]
fn reading_series_reports_scaled_total_and_borrowed_readings() {
    let data = [1, 2, 3];
    let series = probe::reading_series::series(&data, 4);
    assert_eq!(probe::reading_series::count(&series), 3);
    assert_eq!(probe::reading_series::scaled_total(&series), 24);
    // `readings` borrows from the caller's slice, not from the series value.
    let readings = {
        let short_lived = probe::reading_series::series(&data, 1);
        probe::reading_series::readings(&short_lived)
    };
    assert_eq!(readings, &[1, 2, 3]);
}

#[test]
fn config_parse_maps_keys_to_values_last_wins() {
    let parsed =
        probe::config_parse::parse_config("host = local\nplain line\nport= 8089\nhost =remote\n");
    assert_eq!(
        parsed.len(),
        2,
        "no-`=` lines are skipped, repeats collapse"
    );
    assert_eq!(parsed.get("host").map(String::as_str), Some("remote"));
    assert_eq!(parsed.get("port").map(String::as_str), Some("8089"));
}

#[test]
fn labeled_render_emits_one_line_per_item() {
    let rendered = probe::labeled_render::render_labeled("ch", &[10, 20]);
    assert_eq!(rendered, "ch[0] = 10\nch[1] = 20\n");
    // Display, not Debug: strings render without quotes.
    let words = probe::labeled_render::render_labeled("w", &["a", "b"]);
    assert_eq!(words, "w[0] = a\nw[1] = b\n");
}

#[test]
fn allow_filter_keeps_order_and_repeats() {
    let kept = probe::allow_filter::retain_allowed(&[3, 1, 4, 1, 5], &[1, 9]);
    assert_eq!(kept, vec![3, 4, 5]);
    let strings = probe::allow_filter::retain_allowed(
        &["b".to_string(), "a".to_string(), "b".to_string()],
        &["a".to_string()],
    );
    assert_eq!(strings, vec!["b".to_string(), "b".to_string()]);
}

#[test]
fn frame_totals_accumulates_beyond_u32() {
    assert_eq!(probe::frame_totals::total_bytes(&[3, 4, 5]), 12);
    // The 64-bit report is the point: two max-size frames exceed u32::MAX.
    let big = [u32::MAX, u32::MAX];
    assert_eq!(
        probe::frame_totals::total_bytes(&big),
        2 * u64::from(u32::MAX)
    );
}

#[test]
fn keyed_lookup_finds_first_match_or_reports_missing() {
    let entries = [("a", 1), ("b", 2), ("a", 3)];
    assert_eq!(probe::keyed_lookup::lookup(&entries, "a"), Ok(1));
    assert_eq!(
        probe::keyed_lookup::lookup(&entries, "zz"),
        Err(probe::keyed_lookup::LookupError::MissingKey)
    );
}

#[test]
fn slug_lowercases_and_collapses_separators() {
    assert_eq!(
        probe::slug::slugify("  Hello,  World! 42 "),
        "hello-world-42"
    );
    assert_eq!(probe::slug::slugify("---"), "");
    // The slug is freshly built, so it must outlive the title it came from.
    let owned = {
        let title = String::from("Owned Title");
        probe::slug::slugify(&title)
    };
    assert_eq!(owned, "owned-title");
}

#[test]
fn upload_store_writes_a_receipt_and_hands_back_bytes() {
    let upload = probe::upload_store::Upload {
        name: "report.txt".to_string(),
        bytes: vec![0; 128],
    };
    assert_eq!(probe::upload_store::store(upload), "report.txt: 128 bytes");
    let payload = probe::upload_store::Upload {
        name: "p".to_string(),
        bytes: vec![7, 8],
    };
    assert_eq!(payload.into_bytes(), vec![7, 8]);
}

#[test]
fn line_labels_prefixes_every_entry() {
    let labeled = probe::line_labels::label_all("- ".to_string(), &["one", "two"]);
    assert_eq!(labeled, vec!["- one".to_string(), "- two".to_string()]);
}

#[test]
fn command_describe_returns_name_and_summary() {
    let command = probe::command_describe::Command {
        name: "build".to_string(),
        args: vec!["--fast".to_string(), "--quiet".to_string()],
    };
    let (name, summary) = probe::command_describe::describe(command);
    assert_eq!(name, "build");
    assert_eq!(summary, "build takes 2 arg(s)");
}

#[test]
fn ledger_audit_adjusts_counters_and_records_every_delta() {
    let mut ledger = probe::ledger_audit::Ledger::new(vec![10, 20]);
    ledger.adjust(1, 5);
    ledger.push_audit(0);
    ledger.adjust(0, -3);
    assert_eq!(ledger.counters(), &[7, 25]);
    assert_eq!(ledger.audit(), &[5, 0, -3]);
}

#[test]
fn series_extend_clamps_then_appends() {
    let mut series = vec![1, 9];
    probe::series_extend::extend_clamped(&mut series, 5, 2);
    assert_eq!(series, vec![1, 5, 7], "over-cap newest is clamped in place");
    let mut low = vec![3];
    probe::series_extend::extend_clamped(&mut low, 5, 1);
    assert_eq!(low, vec![3, 4], "an in-range newest is left alone");
}

#[test]
fn balance_transfer_moves_amount_between_accounts() {
    let mut balances = [100, 50, 25];
    probe::balance_transfer::transfer(&mut balances, 0, 2, 40);
    assert_eq!(balances, [60, 50, 65]);
}

#[test]
fn rate_counters_clamp_before_reporting() {
    let mut counters = [4, 0];
    assert_eq!(probe::rate_counters::bump(&mut counters, 0, 10, 9), 9);
    assert_eq!(probe::rate_counters::bump(&mut counters, 1, 3, 9), 3);
    assert_eq!(counters, [9, 3]);
}

#[test]
fn account_ledger_reports_the_opening_balance() {
    let mut balances = [100, 7];
    assert_eq!(probe::account_ledger::deposit(&mut balances, 0, 25), 100);
    assert_eq!(balances, [125, 7]);
}

#[test]
fn log_flush_drains_and_terminates() {
    let mut log = String::from("alpha;beta");
    assert_eq!(probe::log_flush::flush(&mut log, ";end"), "alpha;beta;end");
    assert_eq!(log, "", "the log must be left empty");
    assert_eq!(probe::log_flush::flush(&mut log, ""), "");
}

#[test]
fn sensor_history_returns_the_previous_reading() {
    let mut sensor = probe::sensor_history::Sensor::new();
    let first = probe::sensor_history::Reading {
        label: "t".to_string(),
        value: 1,
    };
    let second = probe::sensor_history::Reading {
        label: "t".to_string(),
        value: 2,
    };
    assert_eq!(sensor.record(first.clone()), None);
    assert_eq!(sensor.record(second.clone()), Some(first.clone()));
    assert_eq!(sensor.history(), &[first, second]);
}

#[test]
fn task_queue_absorb_returns_the_prior_queue() {
    let mut queue = vec!["a".to_string()];
    let incoming = ["b".to_string(), "c".to_string()];
    let previous = probe::task_queue::absorb(&mut queue, &incoming);
    assert_eq!(previous, vec!["a".to_string()]);
    assert_eq!(
        queue,
        vec!["a".to_string(), "b".to_string(), "c".to_string()]
    );
}

#[test]
fn label_suffix_appends_in_place() {
    let mut labels = ["a".to_string(), "b".to_string()];
    probe::label_suffix::append_suffix(&mut labels, ":ok");
    assert_eq!(labels, ["a:ok".to_string(), "b:ok".to_string()]);
}

#[test]
fn running_totals_are_prefix_sums() {
    assert_eq!(
        probe::running_totals::running_totals(&[2, -1, 4]),
        vec![2, 1, 5]
    );
    assert_eq!(
        probe::running_totals::running_totals(&[]),
        Vec::<i64>::new()
    );
}

#[test]
fn event_log_records_in_order() {
    let mut log = probe::event_log::EventLog::new();
    assert!(log.is_empty());
    log.record("boot");
    log.record("ready");
    assert_eq!(log.len(), 2);
    assert!(!log.is_empty());
    assert_eq!(log.entries(), &["boot".to_string(), "ready".to_string()]);
}

#[test]
fn pipeline_builds_the_run_report() {
    let report = probe::pipeline::run_report("title = Nightly Run 7\n", &[3, 5], &[100, 200]);
    assert_eq!(report.slug, "nightly-run-7");
    assert_eq!(
        report.rendered,
        "nightly-run-7[0] = 3\nnightly-run-7[1] = 5\n"
    );
    assert_eq!(report.payload_bytes, 300);
    let untitled = probe::pipeline::run_report("", &[], &[]);
    assert_eq!(untitled.slug, "untitled");
    assert_eq!(untitled.rendered, "");
    assert_eq!(untitled.payload_bytes, 0);
}

#[test]
fn pipeline_apply_samples_reports_each_update() {
    let mut channels = [0, 10];
    let updates = probe::pipeline::apply_samples(&mut channels, &[(0, 4), (1, 99), (0, 6)], 50);
    assert_eq!(channels, [6, 50]);
    assert_eq!(updates.len(), 3);
    assert_eq!((updates[0].previous, updates[0].current), (0, 4));
    assert_eq!((updates[1].previous, updates[1].current), (10, 50));
    assert_eq!((updates[2].previous, updates[2].current), (4, 6));
}
