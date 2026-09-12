pub fn evaluate(a: bool, b: bool, c: bool) -> Vec<u8> {
    let mut trace = Vec::new();
    trace.push(1);
    if a {
        trace.push(2);
        if b {
            trace.push(3);
            if c {
                trace.push(4);
            }
        }
    }
    trace
}

pub fn classify(value: u8) -> u8 {
    match value.rotate_left(1) & 3 {
        0 => 10,
        1 => 20,
        _ => 30,
    }
}

pub fn transform(value: i16) -> i16 {
    let clamp = |number: i16| {
        if number < 0 {
            0
        } else if number > 10 {
            10
        } else {
            number
        }
    };
    clamp(value)
}

pub fn calculate(a: i16, b: i16, c: i16) -> i16 {
    (a + b) * (c - 1)
}
