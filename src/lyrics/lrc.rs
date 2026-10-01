//! Minimal LRC parser: `[mm:ss.xx]text`, multiple timestamps per line,
//! metadata tags (`[ar:...]`) ignored.

use super::Line;

pub fn parse(text: &str) -> Vec<Line> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let mut rest = raw.trim();
        let mut times = Vec::new();
        while let Some(stripped) = rest.strip_prefix('[') {
            let Some(end) = stripped.find(']') else { break };
            match parse_time(&stripped[..end]) {
                Some(t) => times.push(t),
                None if times.is_empty() => break, // metadata tag
                None => break,
            }
            rest = stripped[end + 1..].trim_start();
        }
        for t in times {
            out.push(Line { time: t, text: rest.trim().to_string() });
        }
    }
    out.sort_by_key(|l| l.time);
    out
}

fn parse_time(tag: &str) -> Option<i64> {
    let (m, s) = tag.split_once(':')?;
    let m: i64 = m.trim().parse().ok()?;
    let (sec, frac) = match s.split_once(['.', ':']) {
        Some((a, b)) => (a, b),
        None => (s, "0"),
    };
    let sec: i64 = sec.trim().parse().ok()?;
    let frac = frac.trim();
    let digits: String = frac.chars().take(3).collect();
    let mut ms: i64 = digits.parse().ok()?;
    for _ in digits.len()..3 {
        ms *= 10;
    }
    Some(m * 60_000 + sec * 1000 + ms)
}

/// Turns plain text into unsynced lines.
pub fn plain(text: &str) -> Vec<Line> {
    text.lines().map(|l| Line { time: -1, text: l.trim().to_string() }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lrc() {
        let l = parse("[ar:Someone]\n[00:12.34]Hello\n[01:02.5][00:01.00]Twice\n[00:03.00]");
        let t: Vec<_> = l.iter().map(|l| (l.time, l.text.as_str())).collect();
        assert_eq!(t, vec![(1000, "Twice"), (3000, ""), (12340, "Hello"), (62500, "Twice")]);
    }
}
