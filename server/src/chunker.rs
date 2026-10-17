//! Splits the streaming LLM reply into TTS-sized pieces and extracts inline tags.
//!
//! The first piece is cut early (first clause boundary) so audio starts as soon as possible;
//! later pieces are whole sentences, which gives the TTS model enough context for natural
//! prosody while it still runs ahead of playback.
//!
//! Rest as one (voice-joins lab, 2026-10-15): after the first piece, the rest of the reply is
//! held and sent as ONE generation when the reply ends (or once it is long, or after a wait), so
//! the voice doesn't restart its intonation at every sentence. Separately generated sentences
//! were heard as different voices; one generation had no glitches (5% split) and a steadier pitch.
//!
//! Whole reply (voice-consistency lab, 2026-10-16): the first piece is held too, so a reply that
//! the model finishes quickly (median 212 ms after its first clause) is ONE generation: no
//! restart of the voice inside a reply (the short first piece came out ~2 semitones and ~30 %
//! in pace away from the rest), and it streams its first audio in ~0.12 s where the short
//! whole take needed ~1 s. A slow or long reply falls back to first clause + rest.
//!
//! Tags like `[amused]` set the voice tone for the pieces that follow; `[think]` marks a
//! delegation bridge (the first piece is then a whole sentence). Tags are never spoken.

#[derive(Debug, Clone, PartialEq)]
pub struct Piece {
    pub text: String,
    pub tone: Option<String>,
}

pub struct Chunker {
    buf: String,
    emitted: usize,
    first_min_words: usize,
    min_words: usize,
    tone: Option<String>,
    think: bool,
    first_sentence: bool,
    /// Rest-as-one: after the first piece, cut only at a sentence end once this many words are
    /// held (0 = off: every sentence of `min_words` or more is its own piece).
    rest_words: usize,
    /// …or at the last sentence end once this long has passed since the first piece (a slow
    /// model must not leave the player dry).
    rest_wait: Option<std::time::Duration>,
    first_at: Option<std::time::Instant>,
    /// Whole-reply mode: hold the first piece this long after it could have been cut.
    hold_first: Option<std::time::Duration>,
    first_ready_at: std::cell::Cell<Option<std::time::Instant>>,
    /// A tone tag cuts the text before it into its own piece (only useful for models that take a
    /// tone instruction; cloned Base voices ignore it, and every cut is a new voice draw).
    split_tags: bool,
}

const CLAUSE: &[char] = &[',', ';', ':', '.', '!', '?', '—'];
const SENTENCE: &[char] = &['.', '!', '?', '…'];
const MAX_TAG: usize = 32;

impl Chunker {
    pub fn new(first_min_words: usize, min_words: usize) -> Self {
        Self { buf: String::new(), emitted: 0, first_min_words, min_words, tone: None, think: false, first_sentence: false, rest_words: 0, rest_wait: None, first_at: None, hold_first: None, first_ready_at: std::cell::Cell::new(None), split_tags: true }
    }

    /// Cut the first piece at a sentence end instead of the first clause: one prosodic unit,
    /// ~50 ms later first audio.
    pub fn first_sentence(mut self, on: bool) -> Self {
        self.first_sentence = on;
        self
    }

    /// Hold everything after the first piece and send it as one piece (see the module doc).
    pub fn rest_as_one(mut self, max_words: usize, wait_ms: u64) -> Self {
        self.rest_words = max_words;
        self.rest_wait = (wait_ms > 0).then(|| std::time::Duration::from_millis(wait_ms));
        self
    }

    /// Hold the first piece (see the module doc): one piece per reply when the model is quick.
    pub fn whole_reply(mut self, wait_ms: u64) -> Self {
        self.hold_first = (wait_ms > 0).then(|| std::time::Duration::from_millis(wait_ms));
        self
    }

    /// Tone tags set the tone without cutting the text (cloned voices take no tone instruction).
    pub fn tags_split(mut self, on: bool) -> Self {
        self.split_tags = on;
        self
    }

    /// Continue with a tone carried over from an earlier stream (delegation hand-off).
    pub fn with_tone(mut self, tone: Option<String>) -> Self {
        self.tone = tone;
        self
    }

    pub fn tone(&self) -> Option<String> {
        self.tone.clone()
    }

    /// True once a `[think]` tag was seen.
    pub fn think(&self) -> bool {
        self.think
    }

    /// Feed a text delta; returns any pieces that are ready to synthesize.
    pub fn push(&mut self, delta: &str) -> Vec<Piece> {
        self.buf.push_str(delta);
        let mut out = Vec::new();
        loop {
            // Text before an incomplete tag may still be cut; text after it may not.
            let mut limit = self.buf.len();
            if let Some(open) = self.buf.find('[') {
                match self.buf[open..].find(']') {
                    Some(rel) => {
                        let close = open + rel;
                        let tag = self.buf[open + 1..close].trim().to_lowercase();
                        let before: String = self.buf.drain(..=close).collect();
                        if self.split_tags || tag == "think" {
                            // A tag starts a new tone segment: flush what came before it.
                            self.emit(&before[..open], &mut out);
                        } else {
                            // Keep the text; the tag only sets the tone of a piece not started yet.
                            self.buf.insert_str(0, &before[..open]);
                            if !before[..open].trim().is_empty() {
                                continue;
                            }
                        }
                        if tag == "think" {
                            self.think = true;
                        } else if !tag.is_empty() {
                            self.tone = Some(tag);
                        }
                        continue;
                    }
                    None if self.buf.len() - open > MAX_TAG => {
                        // Not a tag after all: drop the bracket and treat it as text.
                        self.buf.remove(open);
                        continue;
                    }
                    None => limit = open,
                }
            }
            match self.find_cut(limit) {
                Some(cut) => {
                    let piece: String = self.buf.drain(..cut).collect();
                    self.emit(&piece, &mut out);
                }
                None => break,
            }
        }
        out
    }

    pub fn finish(&mut self) -> Option<Piece> {
        let rest = std::mem::take(&mut self.buf);
        let rest = rest.split('[').next().unwrap_or_default();
        clean(rest).map(|text| Piece { text, tone: self.tone.clone() })
    }

    fn emit(&mut self, s: &str, out: &mut Vec<Piece>) {
        if let Some(text) = clean(s) {
            out.push(Piece { text, tone: self.tone.clone() });
            self.emitted += 1;
            self.first_at.get_or_insert_with(std::time::Instant::now);
        }
    }

    /// Byte index just after a boundary char that is followed by whitespace (so "3.5" or
    /// a token-split "e.g" never cuts) and whose prefix is long enough.
    fn find_cut(&self, limit: usize) -> Option<usize> {
        if self.emitted == 0 && !self.think {
            if let Some(hold) = self.hold_first {
                return self.held_first_cut(limit, hold);
            }
        }
        let (marks, min) = match (self.emitted, self.think) {
            (0, false) if !self.first_sentence => (CLAUSE, self.first_min_words),
            (0, false) => (SENTENCE, self.first_min_words),
            (0, true) => (SENTENCE, 1),
            _ if self.rest_words > 0 => return self.rest_cut(limit),
            _ => (SENTENCE, self.min_words),
        };
        let text = &self.buf[..limit];
        let mut chars = self.buf.char_indices().peekable();
        while let Some((i, c)) = chars.next() {
            if i >= limit {
                return None;
            }
            if !marks.contains(&c) {
                continue;
            }
            let Some(&(_, next)) = chars.peek() else { return None };
            if !next.is_whitespace() {
                continue;
            }
            let end = i + c.len_utf8();
            if text[..end.min(limit)].split_whitespace().count() >= min {
                return Some(end);
            }
        }
        None
    }
}

impl Chunker {
    /// Whole-reply first cut: nothing until the reply ends (finish), unless it grows long (a
    /// sentence end after `rest_words`) or `hold` has passed since a first clause was ready; then
    /// the last sentence end held so far, else that first clause.
    fn held_first_cut(&self, limit: usize, hold: std::time::Duration) -> Option<usize> {
        let text = &self.buf[..limit];
        let mut clause = None;
        let mut sentences = Vec::new();
        for ((i, c), next) in text.char_indices().zip(text.chars().skip(1)) {
            if !next.is_whitespace() { continue; }
            let end = i + c.len_utf8();
            if CLAUSE.contains(&c) && clause.is_none() && text[..end].split_whitespace().count() >= self.first_min_words { clause = Some(end); }
            if SENTENCE.contains(&c) { sentences.push(end); }
        }
        let clause = clause?;
        let ready = self.first_ready_at.get().unwrap_or_else(|| { let t = std::time::Instant::now(); self.first_ready_at.set(Some(t)); t });
        if self.rest_words > 0 {
            if let Some(&e) = sentences.iter().find(|&&e| text[..e].split_whitespace().count() >= self.rest_words) {
                return Some(e);
            }
        }
        if ready.elapsed() >= hold {
            return Some(sentences.last().copied().filter(|&e| e >= clause).unwrap_or(clause));
        }
        None
    }

    /// Rest-as-one cut: the first sentence end after `rest_words` words, else (once `rest_wait`
    /// has passed since the first piece) the last sentence end held so far.
    fn rest_cut(&self, limit: usize) -> Option<usize> {
        let text = &self.buf[..limit];
        let ends: Vec<usize> = text
            .char_indices()
            .zip(text.chars().skip(1))
            .filter(|((_, c), next)| SENTENCE.contains(c) && next.is_whitespace())
            .map(|((i, c), _)| i + c.len_utf8())
            .collect();
        if let Some(&e) = ends.iter().find(|&&e| text[..e].split_whitespace().count() >= self.rest_words) {
            return Some(e);
        }
        let waited = matches!((self.rest_wait, self.first_at), (Some(w), Some(t)) if t.elapsed() >= w);
        ends.last().copied().filter(|&e| waited && text[..e].split_whitespace().count() >= self.min_words)
    }
}

/// Strip markup the TTS would read aloud (pattern from the vault's Bops voice agent).
fn clean(s: &str) -> Option<String> {
    let mut t = String::with_capacity(s.len());
    for c in s.chars() {
        if !matches!(c, '*' | '_' | '#' | '`' | '>' | '|' | '~' | '[' | ']') {
            t.push(c);
        }
    }
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    (t.chars().any(char::is_alphanumeric)).then_some(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(v: &[Piece]) -> Vec<&str> {
        v.iter().map(|p| p.text.as_str()).collect()
    }

    #[test]
    fn first_clause_then_sentences() {
        let mut c = Chunker::new(2, 4);
        let mut got = Vec::new();
        for d in ["Sure thing, ", "you could try ", "fried rice. It is fast and ", "really tasty. ", "Enjoy!"] {
            got.extend(c.push(d));
        }
        got.extend(c.finish());
        assert_eq!(texts(&got), vec!["Sure thing,", "you could try fried rice.", "It is fast and really tasty.", "Enjoy!"]);
    }

    #[test]
    fn whole_reply_is_one_piece_when_quick() {
        let mut c = Chunker::new(2, 4).rest_as_one(45, 0).whole_reply(5_000).tags_split(false);
        let mut got = Vec::new();
        for d in ["[warm] Sure thing, ", "you could try ", "fried rice. [amused] It is fast and ", "really tasty. ", "Enjoy!"] {
            got.extend(c.push(d));
        }
        assert!(got.is_empty(), "{got:?}");
        got.extend(c.finish());
        assert_eq!(texts(&got), vec!["Sure thing, you could try fried rice. It is fast and really tasty. Enjoy!"]);
        assert_eq!(got[0].tone.as_deref(), Some("warm"));
    }

    #[test]
    fn whole_reply_falls_back_after_the_hold() {
        let mut c = Chunker::new(2, 4).rest_as_one(45, 0).whole_reply(1);
        let mut got = c.push("Okay so, one two three four. Five");
        std::thread::sleep(std::time::Duration::from_millis(5));
        got.extend(c.push(" six."));
        assert_eq!(texts(&got), vec!["Okay so, one two three four."]);
    }

    #[test]
    fn rest_as_one_holds_after_first_piece() {
        let mut c = Chunker::new(2, 4).rest_as_one(45, 0);
        let mut got = Vec::new();
        for d in ["Sure thing, ", "you could try ", "fried rice. It is fast and ", "really tasty. ", "Enjoy!"] {
            got.extend(c.push(d));
        }
        assert_eq!(texts(&got), vec!["Sure thing,"]);
        got.extend(c.finish());
        assert_eq!(texts(&got), vec!["Sure thing,", "you could try fried rice. It is fast and really tasty. Enjoy!"]);
    }

    #[test]
    fn rest_as_one_cuts_long_replies_and_after_the_wait() {
        let long = "One two three four five six. Seven eight nine ten eleven twelve. Thirteen fourteen. ";
        let mut c = Chunker::new(2, 4).rest_as_one(10, 0);
        let mut got = c.push("Okay so, ");
        got.extend(c.push(long));
        assert_eq!(texts(&got), vec!["Okay so,", "One two three four five six. Seven eight nine ten eleven twelve."]);
        let mut c = Chunker::new(2, 4).rest_as_one(45, 1);
        let mut got = c.push("Okay so, one two three four. Five six");
        std::thread::sleep(std::time::Duration::from_millis(5));
        got.extend(c.push(" seven."));
        assert_eq!(texts(&got), vec!["Okay so,", "one two three four."]);
    }

    #[test]
    fn no_cut_inside_numbers() {
        let mut c = Chunker::new(1, 1);
        assert!(c.push("It costs 3.5").is_empty());
        assert_eq!(texts(&c.push(" euros. Ok")), vec!["It costs 3.5 euros."]);
    }

    #[test]
    fn tone_tags_split_and_set_tone() {
        let mut c = Chunker::new(2, 2);
        let mut got = c.push("[empa");
        got.extend(c.push("thetic] Oh, that sounds exhausting. [cur"));
        got.extend(c.push("ious] Is it the workload?"));
        got.extend(c.finish());
        assert_eq!(
            got,
            vec![
                Piece { text: "Oh, that sounds exhausting.".into(), tone: Some("empathetic".into()) },
                Piece { text: "Is it the workload?".into(), tone: Some("curious".into()) },
            ]
        );
    }

    #[test]
    fn think_tag_makes_first_piece_a_sentence() {
        let mut c = Chunker::new(2, 4);
        let got = c.push("[think] [thoughtful] Ooh, okay, let me think that through. Rome");
        assert!(c.think());
        assert_eq!(texts(&got), vec!["Ooh, okay, let me think that through."]);
        assert_eq!(got[0].tone.as_deref(), Some("thoughtful"));
    }
}
