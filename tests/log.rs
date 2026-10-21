use std::fs::OpenOptions;

use blamo::Log;

fn records(log: &Log) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    log.replay(|r| {
        out.push(r.to_vec());
        Ok(())
    })
    .unwrap();
    out
}

fn files(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    names.sort();
    names
}

#[test]
fn records_survive_reopening_in_order() {
    let dir = tempfile::tempdir().unwrap();
    {
        let log = Log::open(dir.path()).unwrap();
        assert!(log.is_empty().unwrap());
        log.append(b"one").unwrap();
        log.append_all([&b"two"[..], &b""[..], &b"three"[..]]).unwrap();
        log.sync().unwrap();
    }
    let log = Log::open(dir.path()).unwrap();
    assert_eq!(records(&log), vec![b"one".to_vec(), b"two".to_vec(), Vec::new(), b"three".to_vec()]);
    assert_eq!(log.checkpoint(), None);
    log.append(b"four").unwrap();
    assert_eq!(records(&log).len(), 5);
}

#[test]
fn a_torn_tail_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    {
        let log = Log::open(dir.path()).unwrap();
        log.append(b"kept").unwrap();
        log.append(b"torn record").unwrap();
    }
    let segment = dir.path().join(&files(dir.path())[0]);
    let length = std::fs::metadata(&segment).unwrap().len();
    OpenOptions::new().write(true).open(&segment).unwrap().set_len(length - 3).unwrap();
    let log = Log::open(dir.path()).unwrap();
    assert_eq!(records(&log), vec![b"kept".to_vec()]);
    log.append(b"after").unwrap(); // appends continue where the valid frames end
    drop(log);
    assert_eq!(records(&Log::open(dir.path()).unwrap()), vec![b"kept".to_vec(), b"after".to_vec()]);
}

#[test]
fn a_corrupt_record_ends_the_last_segment_but_is_an_error_earlier() {
    let dir = tempfile::tempdir().unwrap();
    {
        let log = Log::open(dir.path()).unwrap();
        log.append(b"first").unwrap();
        log.append(b"second").unwrap();
    }
    let segment = dir.path().join(&files(dir.path())[0]);
    let mut bytes = std::fs::read(&segment).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF;
    std::fs::write(&segment, &bytes).unwrap();
    {
        let log = Log::open(dir.path()).unwrap();
        assert_eq!(records(&log), vec![b"first".to_vec()]);
        log.rotate().unwrap();
        log.append(b"third").unwrap();
    }
    // damage the first segment, now followed by another: not a torn tail
    let mut bytes = std::fs::read(&segment).unwrap();
    bytes[9] ^= 0xFF;
    std::fs::write(&segment, &bytes).unwrap();
    assert!(Log::open(dir.path()).is_err());
}

#[test]
fn checkpoints_bound_the_log() {
    let dir = tempfile::tempdir().unwrap();
    {
        let log = Log::open(dir.path()).unwrap();
        log.append(b"a").unwrap();
        let segment = log.rotate().unwrap();
        log.append(b"b").unwrap(); // after the rotation: replayed after the checkpoint
        log.write_checkpoint(segment, b"state with a").unwrap();
        log.append(b"c").unwrap();
        assert_eq!(records(&log), vec![b"b".to_vec(), b"c".to_vec()]);
    }
    assert_eq!(files(dir.path()), vec!["0000000000000001.checkpoint".to_string(), "0000000000000001.log".to_string()]);
    let log = Log::open(dir.path()).unwrap();
    assert_eq!(log.checkpoint().unwrap(), b"state with a");
    assert_eq!(records(&log), vec![b"b".to_vec(), b"c".to_vec()]);
    log.reset(b"everything").unwrap();
    assert!(log.is_empty().unwrap());
    drop(log);
    let log = Log::open(dir.path()).unwrap();
    assert_eq!(log.checkpoint().unwrap(), b"everything");
    assert!(records(&log).is_empty());
    assert!(log.write_checkpoint(1, b"older").is_err());
    assert!(log.write_checkpoint(9, b"not started").is_err());
}

#[test]
fn an_interrupted_checkpoint_leaves_the_previous_one() {
    let dir = tempfile::tempdir().unwrap();
    {
        let log = Log::open(dir.path()).unwrap();
        log.append(b"a").unwrap();
        log.rotate().unwrap();
        log.append(b"b").unwrap();
    }
    // a crash while writing the checkpoint for segment 1: only its temporary file exists
    std::fs::write(dir.path().join("0000000000000001.checkpoint.new"), b"partial").unwrap();
    let log = Log::open(dir.path()).unwrap();
    assert_eq!(log.checkpoint(), None);
    assert_eq!(records(&log), vec![b"a".to_vec(), b"b".to_vec()]);
    assert!(!dir.path().join("0000000000000001.checkpoint.new").exists());
}
