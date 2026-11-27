use postbox_platform::is_reparse_point_or_symlink;
use postbox_store::Store;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Bounded per-sweep directory scan cap (`main.zig:7998, 8022`).
pub const MAX_PRUNE_SCAN_ENTRIES: usize = 4096;

/// Howard Hinnant civil-date algorithm returning days since 1970-01-01 (`media_age.zig:3-12`).
pub const fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m_shift = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * m_shift + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

const fn is_leap_year(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn parse_ascii_digits(bytes: &[u8]) -> Option<i64> {
    if bytes.is_empty() {
        return None;
    }
    let mut val: i64 = 0;
    for &b in bytes {
        if !b.is_ascii_digit() {
            return None;
        }
        val = val.checked_mul(10)?.checked_add(i64::from(b - b'0'))?;
    }
    Some(val)
}

fn parse_civil_date(ts: &str) -> Option<(i64, i64, i64)> {
    let bytes = ts.as_bytes();
    if bytes.len() < 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let year = parse_ascii_digits(&bytes[0..4])?;
    let month = parse_ascii_digits(&bytes[5..7])?;
    let day = parse_ascii_digits(&bytes[8..10])?;
    if year <= 0 || !(1..=12).contains(&month) {
        return None;
    }
    let day_limit = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
        _ => return None,
    };
    if day < 1 || day > day_limit {
        return None;
    }
    Some((year, month, day))
}

/// Exact port of `media_age.zig:14-28`: returns calendar-day difference between `now_unix` and
/// the `YYYY-MM-DD...` timestamp, or `None` if unparseable or invalid date (incl. leap years).
pub fn age_days(timestamp: &str, now_unix: i64) -> Option<i64> {
    let (year, month, day) = parse_civil_date(timestamp)?;
    Some((now_unix / 86_400) - days_from_civil(year, month, day))
}

/// Exact port of `media_age.zig:30-33`: fails OPEN (`true`) on unparseable timestamps so unknown
/// formats never suppress media downloads.
pub fn within_days(timestamp: &str, now_unix: i64, days: i64) -> bool {
    match age_days(timestamp, now_unix) {
        Some(age) => age <= days,
        None => true,
    }
}

/// Exact port of `media_age.zig:35-46`: parses `YYYY-MM-DD?HH?MM?SS` (allowing leap second 60).
pub fn unix_seconds(timestamp: &str) -> Option<i64> {
    let (year, month, day) = parse_civil_date(timestamp)?;
    let bytes = timestamp.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let hour = parse_ascii_digits(&bytes[11..13])?;
    let minute = parse_ascii_digits(&bytes[14..16])?;
    let second = parse_ascii_digits(&bytes[17..19])?;
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// Formats a UTC `YYYY-MM-DDTHH:MM:SSZ` string from Unix seconds using Howard Hinnant's inverse
/// civil-from-days formula (used for 14-day cutoff queries).
pub fn format_utc_rfc3339(unix_secs: i64) -> String {
    let days = unix_secs.div_euclid(86_400);
    let secs_of_day = unix_secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    if m <= 2 {
        y += 1;
    }
    let hour = secs_of_day / 3_600;
    let minute = (secs_of_day % 3_600) / 60;
    let second = secs_of_day % 60;
    format!("{y:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Best-effort updates a cached file's on-disk `mtime` to now so filesystem pruning stays in sync
/// with SQLite `media_cache.last_accessed_at`. Never fails or panics (e.g., on read-only files).
pub fn touch_file_mtime(path: &Path) {
    if is_reparse_point_or_symlink(path) {
        return;
    }
    if let Ok(file) = std::fs::OpenOptions::new().write(true).open(path) {
        let _ = file.set_modified(SystemTime::now());
    }
}

/// Prunes stale files in `dir` whose mtime is strictly older than `max_age_days` (`14` days =
/// `1_209_600` seconds), scanning at most `max_entries` (`4096`) entries and strictly skipping
/// symlinks and Windows NTFS reparse points (`main.zig:7986-8039`).
pub fn prune_stale_cache_dir(
    dir: &Path,
    now_unix: i64,
    max_age_days: i64,
    max_entries: usize,
) -> usize {
    prune_stale_cache_dir_excluding(
        dir,
        now_unix,
        max_age_days,
        max_entries,
        &std::collections::HashSet::new(),
    )
}

fn prune_stale_cache_dir_excluding(
    dir: &Path,
    now_unix: i64,
    max_age_days: i64,
    max_entries: usize,
    protected_paths: &std::collections::HashSet<std::path::PathBuf>,
) -> usize {
    if max_age_days < 0 || !dir.exists() || is_reparse_point_or_symlink(dir) {
        return 0;
    }
    let cutoff_secs = max_age_days.saturating_mul(86_400);
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return 0;
    };

    let mut removed = 0usize;
    for (scanned, entry) in read_dir.flatten().enumerate() {
        if scanned >= max_entries {
            break;
        }
        let path = entry.path();
        if protected_paths.contains(&path) || is_reparse_point_or_symlink(&path) {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let Ok(modified) = meta.modified() else {
            continue;
        };
        let Ok(dur) = modified.duration_since(UNIX_EPOCH) else {
            continue;
        };
        let mtime_unix = dur.as_secs() as i64;
        if now_unix.saturating_sub(mtime_unix) > cutoff_secs && std::fs::remove_file(&path).is_ok()
        {
            removed += 1;
        }
    }
    removed
}

/// Runs synchronized 14-day eviction across BOTH SQLite `media_cache` and the on-disk `hub-media/`
/// directory, safely skipping symlinks/reparse points and protecting live SQLite-tracked files
/// whose `last_accessed_at` is still within the 14-day LRU window.
pub fn evict_expired_media_cache(
    store: &Store,
    cache_root: &Path,
    now_unix: i64,
    max_age_days: i64,
) -> usize {
    let cutoff = now_unix.saturating_sub(max_age_days.saturating_mul(86_400));
    let expired_rels = store
        .evict_expired_media_older_than(cutoff)
        .unwrap_or_default();
    let live_paths: std::collections::HashSet<std::path::PathBuf> = store
        .list_live_media_rel_paths()
        .unwrap_or_default()
        .into_iter()
        .map(|rel| cache_root.join(rel))
        .collect();

    let mut removed = 0usize;
    for rel in expired_rels {
        let full = cache_root.join(&rel);
        if !live_paths.contains(&full)
            && !is_reparse_point_or_symlink(&full)
            && std::fs::remove_file(&full).is_ok()
        {
            removed += 1;
        }
    }
    removed
        + prune_stale_cache_dir_excluding(
            cache_root,
            now_unix,
            max_age_days,
            MAX_PRUNE_SCAN_ENTRIES,
            &live_paths,
        )
}

pub fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_date_leap_years_and_fail_open() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(
            days_from_civil(2024, 2, 29),
            days_from_civil(2024, 3, 1) - 1
        );
        // 2023 is not a leap year; 2000 is; 1900 is not.
        assert_eq!(age_days("2023-02-29T00:00:00Z", 0), None);
        assert!(age_days("2000-02-29T00:00:00Z", 0).is_some());
        assert_eq!(age_days("1900-02-29T00:00:00Z", 0), None);

        let now = unix_seconds("2026-11-26T12:00:00Z").unwrap();
        assert_eq!(age_days("2026-11-12T00:00:00Z", now), Some(14));
        assert!(within_days("2026-11-12T00:00:00Z", now, 14));
        assert!(!within_days("2026-11-11T23:59:59Z", now, 14));
        // Fail open on unparseable timestamp!
        assert!(within_days("not-a-timestamp", now, 14));
        assert!(within_days("", now, 14));
        // Leap second 60 accepted
        assert!(unix_seconds("2026-11-26T23:59:60Z").is_some());
        assert_eq!(unix_seconds("2026-11-26T23:59:61Z"), None);
        assert_eq!(format_utc_rfc3339(now), "2026-11-26T12:00:00Z");
    }

    #[test]
    fn prune_stale_cache_dir_respects_14d_cutoff_and_skips_symlinks() {
        let dir = std::env::temp_dir().join(format!(
            "postbox_media_prune_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file_a = dir.join("old.bin");
        std::fs::write(&file_a, b"data").unwrap();

        #[cfg(unix)]
        let link_path = {
            let link = dir.join("link.bin");
            std::os::unix::fs::symlink(&file_a, &link).unwrap();
            link
        };

        let now = now_unix_secs();
        // With `now + 15 days`, `old.bin` is > 14 days old and gets pruned, while symlink is skipped.
        let future = now + 15 * 86_400;
        let removed = prune_stale_cache_dir(&dir, future, 14, MAX_PRUNE_SCAN_ENTRIES);
        assert_eq!(removed, 1);
        assert!(!file_a.exists());
        #[cfg(unix)]
        assert!(link_path.symlink_metadata().is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn f3_evict_expired_media_cache_preserves_recently_accessed_files_and_touch_updates_mtime() {
        let dir = std::env::temp_dir().join(format!(
            "postbox_media_f3_lru_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open_in_memory().unwrap();
        let now = now_unix_secs();
        let old_time = SystemTime::now() - std::time::Duration::from_secs(15 * 86_400);

        // 1. Live tracked file whose mtime was 15d old, accessed recently in SQLite
        let live_file = dir.join("live_recent.png");
        std::fs::write(&live_file, b"live-data").unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&live_file)
            .unwrap()
            .set_modified(old_time)
            .unwrap();
        store
            .upsert_media_cache(&postbox_store::MediaCacheEntry {
                cache_key: "live-key".into(),
                sha256: "a".repeat(64),
                rel_path: "live_recent.png".into(),
                mime_type: "image/png".into(),
                byte_size: 9,
                last_accessed_at: now,
            })
            .unwrap();

        // 2. Expired tracked file (15d old in both SQLite and disk)
        let expired_file = dir.join("expired.png");
        std::fs::write(&expired_file, b"expired-data").unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&expired_file)
            .unwrap()
            .set_modified(old_time)
            .unwrap();
        store
            .upsert_media_cache(&postbox_store::MediaCacheEntry {
                cache_key: "expired-key".into(),
                sha256: "b".repeat(64),
                rel_path: "expired.png".into(),
                mime_type: "image/png".into(),
                byte_size: 12,
                last_accessed_at: now - 15 * 86_400,
            })
            .unwrap();

        // 3. Untracked 15d-old orphan file on disk
        let orphan_file = dir.join("orphan.bin");
        std::fs::write(&orphan_file, b"orphan-data").unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&orphan_file)
            .unwrap()
            .set_modified(old_time)
            .unwrap();

        // Even BEFORE touching mtime, evict_expired_media_cache MUST protect live_recent.png
        // while evicting expired.png and pruning orphan.bin!
        let removed = evict_expired_media_cache(&store, &dir, now, 14);
        assert_eq!(removed, 2);
        assert!(live_file.exists(), "live SQLite-tracked file must survive");
        assert!(
            !expired_file.exists(),
            "expired tracked file must be evicted"
        );
        assert!(
            !orphan_file.exists(),
            "stale untracked orphan must be pruned"
        );
        assert!(store.get_media_cache("live-key").unwrap().is_some());
        assert!(store.get_media_cache("expired-key").unwrap().is_none());

        // touch_file_mtime refreshes the file's mtime to ~now
        touch_file_mtime(&live_file);
        let refreshed_mtime = std::fs::metadata(&live_file)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert!(
            now.saturating_sub(refreshed_mtime).abs() <= 5,
            "expected mtime refreshed close to now ({now}), got {refreshed_mtime}"
        );

        // touch_file_mtime MUST NOT follow a symlink to mutate an external file's mtime
        #[cfg(unix)]
        {
            let external = dir.join("external_target.bin");
            std::fs::write(&external, b"external").unwrap();
            std::fs::OpenOptions::new()
                .write(true)
                .open(&external)
                .unwrap()
                .set_modified(old_time)
                .unwrap();
            let link = dir.join("symlink_entry.bin");
            std::os::unix::fs::symlink(&external, &link).unwrap();
            touch_file_mtime(&link);
            let ext_mtime = std::fs::metadata(&external)
                .unwrap()
                .modified()
                .unwrap()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64;
            assert!(
                now.saturating_sub(ext_mtime) >= 14 * 86_400,
                "touch_file_mtime must not mutate symlink target mtime (got {ext_mtime}, now {now})"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
