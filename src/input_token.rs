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

pub fn get_slice(vector: &Vec<u8>, slice: Option<(Option<usize>, Option<usize>)>) -> &[u8] {
    match slice {
        None => vector,
        Some(range) => match range {
            (Some(start), Some(end)) => &vector[start..end],
            (Some(start), None) => &vector[start..],
            (None, Some(end)) => &vector[..end],
            (None, None) => &vector[..],
        }
    }
}

pub fn adjust_bytes(source: &Vec<u8>, size: usize) -> Vec<u8> {
    let len = source.len();
    if len == size {
        source.clone()
    } else if size > len {
        let mut buf: Vec<u8> = vec![0; size - len];
        buf.extend_from_slice(source);
        buf
    } else {
        source.split_at(len - size).1.to_vec()
    }
}

pub fn replace_slice(source: &Vec<u8>, value: &Vec<u8>, range: Option<(Option<usize>, Option<usize>)>) -> Vec<u8> {
    if let Some(range) = range {
        let len = source.len();
        let mut v = source.clone();
        match range {
            (Some(start), Some(end)) => v.splice(start..end, adjust_bytes(&value, end - start).into_iter()),
            (Some(start), None) => v.splice(start.., adjust_bytes(&value, len - start).into_iter()),
            (None, Some(end)) => v.splice(..end, adjust_bytes(&value, end).into_iter()),
            (None, None) => v.splice(.., adjust_bytes(&value, len))
        };
        v
    } else {
        adjust_bytes(value, source.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adjust_bytes_test() {
        assert_eq!(adjust_bytes(&vec![1, 2, 3], 3), &[1, 2, 3]);
        assert_eq!(adjust_bytes(&vec![1, 2, 3], 5), &[0, 0, 1, 2, 3]);
        assert_eq!(adjust_bytes(&vec![1, 2, 3], 2), &[2, 3]);
        assert_eq!(adjust_bytes(&vec![1, 2, 3], 1), &[3]);
    }

    #[test]
    fn replace_slice_ok() {
        assert_eq!(replace_slice(&vec![1, 2, 3], &vec![1, 4], None), &[0, 1, 4]);
        assert_eq!(replace_slice(&vec![1], &vec![1, 4], None), &[4]);
        assert_eq!(replace_slice(&vec![1, 2, 3], &vec![0, 0, 0, 1, 4], None), &[0, 1, 4]);
        assert_eq!(replace_slice(&vec![1, 2, 3], &vec![4], None), &[0, 0, 4]);

        assert_eq!(replace_slice(&vec![1, 2, 3], &vec![5, 4], Some((Some(0), Some(2)))), &[5, 4, 3]);
        assert_eq!(replace_slice(&vec![1, 2, 3], &vec![4], Some((Some(0), Some(2)))), &[0, 4, 3]);
        assert_eq!(replace_slice(&vec![1, 2, 3], &vec![0, 0, 0, 5, 4], Some((Some(0), Some(2)))), &[5, 4, 3]);

        assert_eq!(replace_slice(&vec![1, 2, 3], &vec![0, 0, 0, 0, 4], Some((Some(1), Some(2)))), &[1, 4, 3]);

        assert_eq!(replace_slice(&vec![1, 2, 3, 4], &vec![6, 7], Some((None, Some(3)))), &[0, 6, 7, 4]);
        assert_eq!(replace_slice(&vec![1, 2, 3, 4], &vec![6], Some((None, Some(3)))), &[0, 0, 6, 4]);
        assert_eq!(replace_slice(&vec![1, 2, 3, 4], &vec![6, 7, 8, 9], Some((None, Some(3)))), &[7, 8, 9, 4]);

        assert_eq!(replace_slice(&vec![1, 2, 3, 4], &vec![6, 7], Some((Some(1), None))), &[1, 0, 6, 7]);
        assert_eq!(replace_slice(&vec![1, 2, 3, 4], &vec![6], Some((Some(1), None))), &[1, 0, 0, 6]);
        assert_eq!(replace_slice(&vec![1, 2, 3, 4], &vec![0, 6, 7, 8, 9], Some((Some(1), None))), &[1, 7, 8, 9]);

        assert_eq!(replace_slice(&vec![1, 2, 3, 4], &vec![6, 7], Some((None, None))), &[0, 0, 6, 7]);
        assert_eq!(replace_slice(&vec![1, 2, 3, 4], &vec![6, 7, 8, 9, 10, 11], Some((None, None))), &[8, 9, 10, 11]);
    }

    #[test]
    #[should_panic]
    fn replace_slice_incorrect_range() {
        replace_slice(&vec![1], &vec![1, 4], Some((Some(3), Some(0))));
    }

    #[test]
    #[should_panic]
    fn replace_slice_out_of_range() {
        replace_slice(&vec![1], &vec![1, 4], Some((Some(0), Some(5))));
    }

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