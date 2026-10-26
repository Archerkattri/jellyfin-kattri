use kattri_parental::{
    HashParams, MAX_BACKOFF_SECS, Pin, PinError, PinLock, RatingDecision, StoredPin, Unlock,
    check_rating, generate_salt,
};

/// Cheapest valid Argon2 cost, to keep the suite fast. Production uses `OWASP_MINIMUM`.
fn fast() -> HashParams {
    match HashParams::new(8, 1, 1) {
        Ok(p) => p,
        Err(err) => panic!("test params should be valid: {err}"),
    }
}

fn pin(digits: &str) -> Pin {
    match Pin::parse(digits) {
        Ok(p) => p,
        Err(err) => panic!("test PIN should parse: {err}"),
    }
}

fn locked_with(digits: &str) -> PinLock {
    let mut lock = PinLock::new(None);
    if let Err(err) = lock.set_pin(&pin(digits), fast(), [7u8; 16]) {
        panic!("set_pin should succeed: {err}");
    }
    lock
}

fn stored_for(digits: &str) -> StoredPin {
    match StoredPin::create(&pin(digits), fast(), [9u8; 16]) {
        Ok(s) => s,
        Err(err) => panic!("create should succeed: {err}"),
    }
}

// ---- PIN format ----

#[test]
fn pin_accepts_four_to_eight_digits_only() {
    for ok in ["1234", "12345678", "0000"] {
        assert!(Pin::parse(ok).is_ok(), "{ok} should parse");
    }
    for bad in ["", "123", "123456789", "12a4", " 1234", "1234 ", "١٢٣٤"] {
        assert_eq!(
            Pin::parse(bad).err(),
            Some(PinError::Format),
            "{bad:?} should be refused"
        );
    }
}

#[test]
fn parse_errors_never_echo_the_input() {
    let err = match Pin::parse("9x9x") {
        Ok(_) => panic!("invalid PIN should not parse"),
        Err(err) => err,
    };
    let shown = format!("{err} {err:?}");
    assert!(
        !shown.contains("9x9x"),
        "error text must not echo the input: {shown}"
    );
}

// ---- set, verify, lockout ----

#[test]
fn set_then_verify_correct_pin_unlocks() {
    let mut lock = locked_with("4821");
    assert!(lock.is_protected());
    assert_eq!(lock.try_unlock("4821", 0), Ok(Unlock::Unlocked));
    assert_eq!(lock.failures(), 0);
}

#[test]
fn wrong_pin_counts_failures_and_starts_a_one_second_delay() {
    let mut lock = locked_with("4821");
    assert_eq!(
        lock.try_unlock("4822", 0),
        Ok(Unlock::WrongPin {
            failures: 1,
            retry_after_secs: 1
        })
    );
    assert_eq!(
        lock.try_unlock("0000", 1),
        Ok(Unlock::WrongPin {
            failures: 2,
            retry_after_secs: 2
        })
    );
    assert_eq!(lock.failures(), 2);
}

#[test]
fn malformed_attempt_counts_as_wrong() {
    let mut lock = locked_with("4821");
    assert_eq!(
        lock.try_unlock("48", 0),
        Ok(Unlock::WrongPin {
            failures: 1,
            retry_after_secs: 1
        })
    );
}

#[test]
fn unprotected_profile_is_never_locked() {
    let mut lock = PinLock::new(None);
    assert!(!lock.is_protected());
    assert_eq!(lock.try_unlock("0000", 0), Ok(Unlock::NotProtected));
}

#[test]
fn delay_refuses_attempts_and_refused_attempts_are_not_counted() {
    let mut lock = locked_with("4821");
    assert_eq!(
        lock.try_unlock("0000", 100),
        Ok(Unlock::WrongPin {
            failures: 1,
            retry_after_secs: 1
        })
    );
    // Even the correct PIN is refused during the delay, and the refusal is not counted.
    assert_eq!(
        lock.try_unlock("4821", 100),
        Ok(Unlock::LockedOut {
            retry_after_secs: 1
        })
    );
    assert_eq!(
        lock.try_unlock("0000", 100),
        Ok(Unlock::LockedOut {
            retry_after_secs: 1
        })
    );
    assert_eq!(lock.failures(), 1);
}

#[test]
fn delay_grows_one_second_per_failure_and_stops_at_thirty() {
    let mut lock = locked_with("4821");
    let mut now = 0u64;
    for failure in 1..=40u32 {
        let expected = u64::from(failure).min(MAX_BACKOFF_SECS);
        assert_eq!(
            lock.try_unlock("0000", now),
            Ok(Unlock::WrongPin {
                failures: failure,
                retry_after_secs: expected
            })
        );
        now += expected;
    }
    assert_eq!(lock.failures(), 40);
    assert_eq!(MAX_BACKOFF_SECS, 30);
}

#[test]
fn correct_pin_after_the_delay_clears_the_counter() {
    let mut lock = locked_with("4821");
    // Each wrong attempt is made when the previous delay has ended: t=0, 1, 3 and 6.
    for now in [0u64, 1, 3, 6] {
        let _ = lock.try_unlock("0000", now);
    }
    // The fourth failure at t=6 starts a delay of four seconds, ending at t=10.
    assert_eq!(lock.failures(), 4);
    assert_eq!(
        lock.try_unlock("4821", 7),
        Ok(Unlock::LockedOut {
            retry_after_secs: 3
        })
    );
    assert_eq!(lock.try_unlock("4821", 10), Ok(Unlock::Unlocked));
    assert_eq!(lock.failures(), 0);
    assert_eq!(lock.attempt_state().locked_until, None);
}

#[test]
fn clear_removes_the_local_pin_and_lock_state() {
    let mut lock = locked_with("4821");
    for _ in 0..5 {
        let _ = lock.try_unlock("0000", 0);
    }
    lock.clear();
    assert!(!lock.is_protected());
    assert_eq!(lock.try_unlock("0000", 1), Ok(Unlock::NotProtected));
}

#[test]
fn random_salt_pin_round_trips() {
    let mut lock = PinLock::new(None);
    if let Err(err) = lock.set_pin_random(&pin("3091"), fast()) {
        panic!("set_pin_random should succeed: {err}");
    }
    assert_eq!(lock.try_unlock("3091", 0), Ok(Unlock::Unlocked));
    assert!(generate_salt().is_ok());
}

#[test]
fn production_cost_parameters_verify() {
    let mut lock = PinLock::new(None);
    if let Err(err) = lock.set_pin(&pin("5150"), HashParams::OWASP_MINIMUM, [3u8; 16]) {
        panic!("set_pin should succeed: {err}");
    }
    assert_eq!(lock.try_unlock("5150", 0), Ok(Unlock::Unlocked));
    assert_eq!(
        lock.try_unlock("5151", 0),
        Ok(Unlock::WrongPin {
            failures: 1,
            retry_after_secs: 1
        })
    );
}

// ---- stored hash and record ----

#[test]
fn stored_pin_matches_only_the_right_digits() {
    let stored = stored_for("8642");
    assert!(stored.matches(&pin("8642")).unwrap_or(false));
    assert!(!stored.matches(&pin("8643")).unwrap_or(true));
}

#[test]
fn record_round_trips_and_keeps_its_parameters() {
    let stored = stored_for("8642");
    let record = stored.to_record();
    assert!(
        record.starts_with("argon2id$v=19$m=8,t=1,p=1$"),
        "record: {record}"
    );
    let parsed = match StoredPin::from_record(&record) {
        Ok(s) => s,
        Err(err) => panic!("record should parse: {err}"),
    };
    assert_eq!(parsed, stored);
    assert_eq!(parsed.params(), fast());
    assert!(parsed.matches(&pin("8642")).unwrap_or(false));
    assert_eq!(parsed.to_record(), record);
}

#[test]
fn record_never_contains_the_pin() {
    let record = stored_for("7391").to_record();
    assert!(
        !record.contains("7391"),
        "record must not contain the PIN: {record}"
    );
}

#[test]
fn malformed_records_are_rejected() {
    let good = stored_for("8642").to_record();
    let parts: Vec<&str> = good.split('$').collect();
    let [alg, ver, costs, salt, digest] = parts.as_slice() else {
        panic!("record should have five fields: {good}");
    };
    let bad = [
        format!("argon2i${ver}${costs}${salt}${digest}"),
        format!("{alg}$v=16${costs}${salt}${digest}"),
        format!("{alg}${ver}$m=8,t=1${salt}${digest}"),
        format!("{alg}${ver}$m=0,t=1,p=1${salt}${digest}"),
        format!("{alg}${ver}${costs}${}${digest}", &salt[2..]),
        format!("{alg}${ver}${costs}${salt}${}", "zz".repeat(32)),
        format!("{alg}${ver}${costs}${salt}"),
        String::new(),
    ];
    for record in bad {
        assert_eq!(
            StoredPin::from_record(&record).err(),
            Some(PinError::Record),
            "{record} should be rejected"
        );
    }
}

#[test]
fn hash_params_are_validated() {
    assert_eq!(HashParams::new(0, 1, 1).err(), Some(PinError::Params));
    assert_eq!(HashParams::new(8, 0, 1).err(), Some(PinError::Params));
    assert_eq!(
        HashParams::new(u32::MAX, 1, 1).err(),
        Some(PinError::Params)
    );
    assert_eq!(
        HashParams::new(8, u32::MAX, 1).err(),
        Some(PinError::Params)
    );
    assert_eq!(
        HashParams::new(8, 1, u32::MAX).err(),
        Some(PinError::Params)
    );
    assert!(HashParams::new(8, 1, 1).is_ok());
}

#[test]
fn records_with_resource_exhaustion_costs_are_rejected_before_hashing() {
    let malicious = "argon2id$v=19$m=4294967295,t=1,p=1$00000000000000000000000000000000$0000000000000000000000000000000000000000000000000000000000000000";
    assert_eq!(
        StoredPin::from_record(malicious).err(),
        Some(PinError::Record)
    );
}

// ---- redaction ----

#[test]
fn pin_debug_is_redacted() {
    let shown = format!("{:?}", pin("2468"));
    assert_eq!(shown, "Pin(<redacted>)");
    assert!(!shown.contains("2468"));
}

#[test]
fn stored_pin_debug_is_redacted() {
    let stored = stored_for("2468");
    let record = stored.to_record();
    let shown = format!("{stored:?}");
    assert_eq!(shown, "StoredPin(<redacted>)");
    let digest_hex = match record.rsplit('$').next() {
        Some(hex) => hex,
        None => panic!("record has a digest field"),
    };
    assert!(!shown.contains(digest_hex));
}

#[test]
fn lock_debug_never_shows_the_record() {
    let lock = locked_with("2468");
    let record = match lock.stored() {
        Some(stored) => stored.to_record(),
        None => panic!("lock should hold a PIN"),
    };
    let shown = format!("{lock:?}");
    assert!(shown.contains("protected: true"), "shows state: {shown}");
    assert!(!shown.contains(&record));
    assert!(!shown.contains("2468"));
}

// ---- rating ceiling ----

#[test]
fn rating_ceiling_allows_at_or_below_and_denies_above() {
    assert_eq!(check_rating(Some(13), Some(10)), RatingDecision::Allow);
    assert_eq!(check_rating(Some(13), Some(13)), RatingDecision::Allow);
    assert_eq!(check_rating(Some(13), Some(17)), RatingDecision::Block);
}

#[test]
fn rating_without_ceiling_allows_everything() {
    assert_eq!(check_rating(None, Some(18)), RatingDecision::Allow);
    assert_eq!(check_rating(None, None), RatingDecision::Allow);
}

#[test]
fn missing_server_rating_is_shown_and_flagged() {
    assert_eq!(check_rating(Some(13), None), RatingDecision::AllowUnrated);
}
