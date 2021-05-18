use std::convert::TryInto;

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

pub fn bytes_to_u128(source: &Vec<u8>) -> u128 {
    u128::from_be_bytes(adjust_bytes(source, U128_SIZE).try_into().unwrap())
}

fn adjust_bytes(source: &Vec<u8>, size: usize) -> Vec<u8> {
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
}