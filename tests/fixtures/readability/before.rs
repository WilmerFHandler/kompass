pub fn guard_clauses(value: i32) -> i32 {
    if value > 0 {
        if value < 10 {
            return value.saturating_mul(2);
        } else {
            return 10;
        }
    } else {
        return 0;
    }
}

pub fn named_intermediates(value: i32) -> i32 {
    value.saturating_mul(3).clamp(-100, 100).saturating_add(7)
}

pub fn useful_extraction(value: i32) -> i32 {
    if value >= 0 {
        if value <= 100 {
            return value.saturating_mul(2);
        }
    }
    0
}

pub fn pointless_wrapper(value: i32) -> i32 {
    value.wrapping_mul(2).wrapping_add(1)
}

pub fn deduplication(value: i32) -> i32 {
    if value >= 0 {
        value.wrapping_mul(2)
    } else {
        value.wrapping_mul(2)
    }
}

pub fn duplicate_left(value: i32) -> i32 {
    if value > 0 {
        value.saturating_mul(2).saturating_add(1)
    } else {
        0
    }
}

pub fn duplicate_right(value: i32) -> i32 {
    if value > 0 {
        value.saturating_mul(2).saturating_add(1)
    } else {
        0
    }
}

pub fn loop_alternative(values: &[i32]) -> i32 {
    let mut total: i32 = 0;
    for value in values {
        total = total.saturating_add(*value);
    }
    total
}

pub fn match_alternative(value: u8) -> i32 {
    if value == 0 {
        10
    } else if value == 1 {
        20
    } else {
        30
    }
}

pub fn formatting(value: i32) -> i32 {
    value.saturating_add(4).saturating_sub(2)
}

pub fn branch_free(value: i64) -> i64 {
    value * 31 + 17
}
