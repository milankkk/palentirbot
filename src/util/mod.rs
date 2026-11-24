use std::mem;

pub use self::ext::*;

pub mod builder;
pub mod constants;
pub mod datetime;
pub mod fs;
pub mod hasher;
pub mod interaction;
pub mod numbers;
pub mod str;

mod ext;

/// "How many replace/delete/insert operations are necessary to morph one word into the other?"
///
/// Returns `(distance, max word length)`.
pub fn levenshtein_distance<'w>(mut word_a: &'w str, mut word_b: &'w str) -> (usize, usize) {
    let m = word_a.chars().count();
    let mut n = word_b.chars().count();

    if m > n {
        mem::swap(&mut word_a, &mut word_b);
        n = m;
    }

    let mut costs: Vec<usize> = (0..=n).collect();

    for (a, i) in word_a.chars().zip(1usize..) {
        let mut last_val = i;

        for (b, j) in word_b.chars().zip(1usize..) {
            let new_val = if a == b {
                costs[j - 1]
            } else {
                costs[j - 1].min(last_val).min(costs[j]) + 1
            };

            costs[j - 1] = last_val;
            last_val = new_val;
        }

        costs[n] = last_val;
    }

    (costs[n], n)
}

#[cfg(test)]
mod tests {
    use super::levenshtein_distance;

    #[test]
    fn test_levenshtein_distance() {
        assert_eq!(levenshtein_distance("", ""), (0, 0));
        assert_eq!(levenshtein_distance("skin", "skin"), (0, 4));
        assert_eq!(levenshtein_distance("kitten", "sitting"), (3, 7));
        assert_eq!(levenshtein_distance("résumé", "resume"), (2, 6));
    }
}
