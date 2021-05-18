use lazy_static::lazy_static;
use regex::Regex;

#[derive(PartialEq, Debug)]
pub enum InputToken {
    Address(String, Option<(Option<usize>, Option<usize>)>),
    Negation,
    None,
}

fn to_int(m: Option<regex::Match>) -> Option<usize> {
    if let Some(m) = m {
        if !m.range().is_empty() {
            return m.as_str().parse::<usize>().ok();
        }
    }
    None
}

pub fn consume_token(input: &mut String) -> InputToken {
    lazy_static! {
        static ref RE: Regex = Regex::new(r"^(?:/)?([^/\[\s]+)(?:\[(\d*)(?:..(\d*))?\])?(?:/)?").unwrap();
    }

    let cloned = input.clone();
    for cap in RE.captures_iter(cloned.as_str()) {
        if let Some(addr) = cap.get(1) {
            if addr.range().is_empty() {
                continue;
            } else {
                *input = input.get(cap.get(0).unwrap().end()..).unwrap().to_string();
                return match addr.as_str() {
                    "!" => InputToken::Negation,
                    _ => InputToken::Address(addr.as_str().to_string(), {
                        let start = cap.get(2);
                        let end = cap.get(3);
                        if start.is_some() || end.is_some() {
                            Some(if let Some(s) = to_int(start) {
                                (Some(s),
                                 if let Some(e) = end {
                                     e.as_str().parse::<usize>().ok()
                                 } else {
                                     Some(s + 1)
                                 })
                            } else {
                                (None, to_int(end))
                            })
                        } else {
                            None
                        }
                    })
                };
            }
        }
    };
    InputToken::None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negation_token() {
        let input = &mut "!/212[51..75]/32".to_string();
        assert_eq!(consume_token(input), InputToken::Negation);
        assert_eq!(input, "212[51..75]/32");

        let input = &mut "!/".to_string();
        assert_eq!(consume_token(input), InputToken::Negation);
        assert_eq!(input, "");
    }

    #[test]
    fn address_token() {
        let input = &mut "/212[51..75]/32".to_string();
        assert_eq!(consume_token(input), InputToken::Address("212".to_string(), Some((Some(51), Some(75)))));
        assert_eq!(input, "32");

        let input = &mut "212[51..75]/32".to_string();
        assert_eq!(consume_token(input), InputToken::Address("212".to_string(), Some((Some(51), Some(75)))));
        assert_eq!(input, "32");

        let input = &mut "212/32".to_string();
        assert_eq!(consume_token(input), InputToken::Address("212".to_string(), None));
        assert_eq!(input, "32");

        let input = &mut "212".to_string();
        assert_eq!(consume_token(input), InputToken::Address("212".to_string(), None));
        assert_eq!(input, "");
    }

    #[test]
    fn range_token() {
        let input = &mut "1[51..75]/32".to_string();
        assert_eq!(consume_token(input), InputToken::Address("1".to_string(), Some((Some(51), Some(75)))));
        assert_eq!(input, "32");

        let input = &mut "/2[51..75]/32".to_string();
        assert_eq!(consume_token(input), InputToken::Address("2".to_string(), Some((Some(51), Some(75)))));
        assert_eq!(input, "32");

        let input = &mut "/3[51..]/32".to_string();
        assert_eq!(consume_token(input), InputToken::Address("3".to_string(), Some((Some(51), None))));
        assert_eq!(input, "32");

        let input = &mut "/4[..75]/32".to_string();
        assert_eq!(consume_token(input), InputToken::Address("4".to_string(), Some((None, Some(75)))));
        assert_eq!(input, "32");

        let input = &mut "/5[6]/32".to_string();
        assert_eq!(consume_token(input), InputToken::Address("5".to_string(), Some((Some(6), Some(7)))));
        assert_eq!(input, "32");

        let input = &mut "/6[..]/32".to_string();
        assert_eq!(consume_token(input), InputToken::Address("6".to_string(), Some((None, None))));
        assert_eq!(input, "32");

        let input = &mut "/7[]/32".to_string();
        assert_eq!(consume_token(input), InputToken::Address("7".to_string(), Some((None, None))));
        assert_eq!(input, "32");
    }

    #[test]
    fn consume_source_string() {
        let input = &mut "/32/988".to_string();
        assert_eq!(consume_token(input), InputToken::Address("32".to_string(), None));
        assert_eq!(input, "988");

        let input = &mut "212/32/988".to_string();
        assert_eq!(consume_token(input), InputToken::Address("212".to_string(), None));
        assert_eq!(input, "32/988");

        let input = &mut "212".to_string();
        assert_eq!(consume_token(input), InputToken::Address("212".to_string(), None));
        assert_eq!(input, "");
        assert_eq!(consume_token(input), InputToken::None);
    }
}