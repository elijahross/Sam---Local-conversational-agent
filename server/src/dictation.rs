//! Dictation mode: the transcript of a held dictation key, cleaned for insertion into any text
//! field — filler words out, spoken layout commands applied, sentence case. Deterministic and
//! instant (no LLM): the words stay the user's own.

/// Cleans a dictated transcript (English and German fillers / commands).
pub fn clean(raw: &str) -> String {
    let mut t = format!(" {} ", raw.trim());
    // Spoken layout commands.
    for (cmd, rep) in [(" new paragraph", "\n\n"), (" neuer absatz", "\n\n"), (" new line", "\n"), (" neue zeile", "\n")] {
        let re = regex::Regex::new(&format!(r"(?i){}[.,!?]?", regex::escape(cmd))).unwrap();
        t = re.replace_all(&t, rep).into_owned();
    }
    // Filler words, with the comma that usually follows them.
    let words = regex::Regex::new(r"(?i)\b(um+|uh+|uhm+|erm+|hmm+|ähm+|äh+|öhm+)\b[,.]?\s*").unwrap();
    t = words.replace_all(&t, "").into_owned();
    // Tidy spaces around punctuation and line breaks.
    t = regex::Regex::new(r"[ \t]+").unwrap().replace_all(&t, " ").into_owned();
    t = regex::Regex::new(r" *\n *").unwrap().replace_all(&t, "\n").into_owned();
    t = regex::Regex::new(r" ([,.!?;:])").unwrap().replace_all(&t, "$1").into_owned();
    let t = t.trim_matches(|c: char| c == ' ').to_string();
    // Sentence case after line breaks and at the start.
    let mut out = String::with_capacity(t.len());
    let mut cap = true;
    for c in t.chars() {
        if cap && c.is_alphabetic() {
            out.extend(c.to_uppercase());
            cap = false;
        } else {
            out.push(c);
        }
        if c == '\n' {
            cap = true;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn cleans_dictation() {
        assert_eq!(super::clean("um, so I think, uh, we should meet on Monday."), "So I think, we should meet on Monday.");
        assert_eq!(super::clean("dear Anna new line thanks for the invite new paragraph best Elijah"), "Dear Anna\nThanks for the invite\n\nBest Elijah");
        assert_eq!(super::clean("ähm also wir treffen uns morgen."), "Also wir treffen uns morgen.");
        assert_eq!(super::clean("the umbrella is here"), "The umbrella is here", "words containing filler letters stay");
    }
}
