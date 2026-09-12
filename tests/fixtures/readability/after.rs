pub fn guard_clauses(value: i32) -> i32 {
    if value <= 0 {
        return 0;
    }
    if value >= 10 {
        return 10;
    }
    value.saturating_mul(2)
}

pub fn named_intermediates(value: i32) -> i32 {
    let scaled = value.saturating_mul(3);
    let bounded = scaled.clamp(-100, 100);
    bounded.saturating_add(7)
}

fn in_range(value: i32) -> bool {
    value >= 0 && value <= 100
}

pub fn useful_extraction(value: i32) -> i32 {
    if in_range(value) {
        value.saturating_mul(2)
    } else {
        0
    }
}

fn apply_offset(value: i32) -> i32 {
    value.wrapping_mul(2).wrapping_add(1)
}

pub fn pointless_wrapper(value: i32) -> i32 {
    apply_offset(value)
}

pub fn deduplication(value: i32) -> i32 {
    value.wrapping_mul(2)
}

fn shared_positive(value: i32) -> i32 {
    if value > 0 {
        value.saturating_mul(2).saturating_add(1)
    } else {
        0
    }
}

pub fn duplicate_left(value: i32) -> i32 {
    shared_positive(value)
}

pub fn duplicate_right(value: i32) -> i32 {
    shared_positive(value)
}

pub fn loop_alternative(values: &[i32]) -> i32 {
    values.iter().copied().fold(0, i32::saturating_add)
}

pub fn match_alternative(value: u8) -> i32 {
    match value {
        0 => 10,
        1 => 20,
        _ => 30,
    }
}

pub fn formatting(value: i32) -> i32 {
    value.saturating_add(4).saturating_sub(2)
}

pub fn branch_free(value: i64) -> i64 {
    let scaled = value * 31;
    scaled + 17
}
