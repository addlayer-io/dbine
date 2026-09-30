//! A minimal reader of the Prometheus text format (`/metrics`): samples by
//! name, labels kept as text, values as `f64`.

pub struct Samples(Vec<(String, String, f64)>);

impl Samples {
    pub fn parse(text: &str) -> Self {
        let mut out = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (head, rest) = match line.find('{') {
                Some(open) => match line[open..].find('}') {
                    Some(close) => (&line[..open], (&line[open + 1..open + close], line[open + close + 1..].trim())),
                    None => continue,
                },
                None => match line.split_once(char::is_whitespace) {
                    Some((n, v)) => (n, ("", v.trim())),
                    None => continue,
                },
            };
            let (labels, value) = rest;
            let value = value.split_whitespace().next().unwrap_or("");
            let v = match value {
                "+Inf" => f64::INFINITY,
                "-Inf" => f64::NEG_INFINITY,
                v => match v.parse::<f64>() {
                    Ok(v) => v,
                    Err(_) => continue,
                },
            };
            out.push((head.to_string(), labels.to_string(), v));
        }
        Self(out)
    }

    /// Sum of every sample of a metric (all label sets); `None` if absent.
    pub fn sum(&self, name: &str) -> Option<f64> {
        let mut any = false;
        let mut total = 0.0;
        for (n, _, v) in &self.0 {
            if n == name {
                any = true;
                total += v;
            }
        }
        any.then_some(total)
    }

    /// Sum of the samples whose labels contain `label` (e.g. `type="unary"`).
    pub fn sum_where(&self, name: &str, label: &str) -> Option<f64> {
        let mut any = false;
        let mut total = 0.0;
        for (n, l, v) in &self.0 {
            if n == name && l.contains(label) {
                any = true;
                total += v;
            }
        }
        any.then_some(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_sum_over_labels() {
        let s = Samples::parse(
            "# HELP x\n# TYPE x counter\nx 3\nreq{type=\"unary\",a=\"b c\"} 2\nreq{type=\"stream\"} 5 1712345\nq 2.147483648e+09\nbad{x\n",
        );
        assert_eq!(s.sum("x"), Some(3.0));
        assert_eq!(s.sum("req"), Some(7.0));
        assert_eq!(s.sum_where("req", "type=\"unary\""), Some(2.0));
        assert_eq!(s.sum("q"), Some(2147483648.0));
        assert_eq!(s.sum("nope"), None);
    }
}
