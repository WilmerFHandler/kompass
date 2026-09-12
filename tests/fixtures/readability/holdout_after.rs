pub fn evaluate(a: bool, b: bool, c: bool) -> Vec<u8> {
    let mut trace = Vec::new();
    trace.push(1);
    if !a {
        return trace;
    }
    trace.push(2);
    if !b {
        return trace;
    }
    trace.push(3);
    if !c {
        return trace;
    }
    trace.push(4);
    trace
}

pub fn classify(value: u8) -> u8 {
    let tag = value.rotate_left(1) & 3;
    match tag {
        0 => 10,
        1 => 20,
        _ => 30,
    }
}

pub fn transform(value: i16) -> i16 {
    fn clamp(number: i16) -> i16 {
        if number < 0 {
            0
        } else if number > 10 {
            10
        } else {
            number
        }
    }
    clamp(value)
}

pub fn calculate(a: i16, b: i16, c: i16) -> i16 {
    let sum = a + b;
    let multiplier = c - 1;
    let result = sum * multiplier;
    result
}
