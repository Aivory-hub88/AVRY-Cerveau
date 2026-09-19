//! Deterministic small-talk detector: is the WHOLE user message a greeting, a
//! thank-you or a farewell, with no request in it?
//!
//! Used to skip work that buys nothing for such a turn (per-turn memory/graph
//! recall, post-turn consolidation). It is a pure lexicon match -- no model call,
//! no I/O -- and deliberately conservative in the direction that is cheap to get
//! wrong: a false positive only loses recalled context for one turn (tools stay
//! available), a false negative just takes the normal path.
//!
//! What it must NOT do is treat a *confirmation* as small talk. In Cerveau a bare
//! "ya" / "oke" / "lanjut" continues a pending action (a drafted email awaiting
//! "kirim"), so none of those words are in the lexicon.

/// Words that make a message small talk when present (greetings, thanks, farewells).
const CORE: &[&str] = &[
    // greetings
    "halo",
    "hallo",
    "hai",
    "hi",
    "hello",
    "hey",
    "helo",
    "pagi",
    "siang",
    "sore",
    "malam",
    "selamat",
    "assalamualaikum",
    "salam",
    "permisi",
    "morning",
    "afternoon",
    "evening",
    "kabar",
    "how",
    "are",
    "you",
    // thanks
    "terima",
    "kasih",
    "makasih",
    "trims",
    "thanks",
    "thank",
    "thx",
    "tq",
    // farewells
    "bye",
    "goodbye",
    "dadah",
    "sampai",
    "jumpa",
    "nanti",
    "besok",
    "see",
];

/// Address forms and softeners that may accompany small talk but never make a
/// message small talk on their own ("ya" alone is a confirmation).
const FILLERS: &[&str] = &[
    "ya",
    "yaa",
    "dong",
    "deh",
    "nih",
    "sih",
    "kak",
    "kakak",
    "pak",
    "bu",
    "ibu",
    "bapak",
    "mas",
    "mbak",
    "bro",
    "sis",
    "min",
    "admin",
    "there",
    "all",
    "semua",
    "semuanya",
    "tim",
    "team",
    "lagi",
    "apa",
    "gimana",
    "bagaimana",
    "kabarnya",
    "sore",
    "later",
    "again",
    "nice",
    "meet",
    "to",
    "the",
    "a",
];

/// Words that mean the message contains a request, even next to a greeting
/// ("halo, tolong kirim ..."). Any of these disqualifies the message.
const COMMAND_WORDS: &[&str] = &[
    "tolong",
    "please",
    "bantu",
    "help",
    "kirim",
    "send",
    "buat",
    "buatkan",
    "create",
    "cari",
    "search",
    "find",
    "cek",
    "check",
    "lihat",
    "tampilkan",
    "show",
    "list",
    "daftar",
    "tambah",
    "add",
    "hapus",
    "delete",
    "update",
    "ubah",
    "ganti",
    "balas",
    "reply",
    "jadwal",
    "schedule",
    "email",
    "mail",
    "surel",
    "inbox",
    "lead",
    "task",
    "tugas",
    "invoice",
    "faktur",
    "tiket",
    "ticket",
    "laporan",
    "report",
    "status",
    "draft",
    "simpan",
    "save",
    "ingat",
    "remember",
    "berapa",
    "siapa",
    "kapan",
    "dimana",
    "mana",
    "kenapa",
    "mengapa",
    "apakah",
    "what",
    "who",
    "when",
    "where",
    "why",
    "can",
    "could",
    "would",
    "ada",
    "adakah",
];

/// Longest message (in words) still considered small talk.
const MAX_WORDS: usize = 6;

/// True iff `message` is nothing but small talk.
#[must_use]
pub fn is_smalltalk(message: &str) -> bool {
    let lowered = message.to_lowercase();
    // Anything carrying data is a request: digits, links, handles, e-mail addresses.
    if lowered.chars().any(|c| c.is_ascii_digit())
        || lowered.contains("http")
        || lowered.contains('@')
        || lowered.contains('/')
    {
        return false;
    }
    // Words only: drop punctuation and emoji, split on whitespace.
    let words: Vec<String> = lowered
        .split(|c: char| !c.is_alphabetic())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect();
    if words.is_empty() || words.len() > MAX_WORDS {
        return false;
    }
    if words.iter().any(|w| COMMAND_WORDS.contains(&w.as_str())) {
        return false;
    }
    if !words.iter().any(|w| CORE.contains(&w.as_str())) {
        return false;
    }
    // Everything that is not lexicon is at most ONE leftover word (an addressee:
    // "halo lex", "selamat pagi aira"), and it must look like a name.
    let leftovers = words
        .iter()
        .filter(|w| !CORE.contains(&w.as_str()) && !FILLERS.contains(&w.as_str()))
        .count();
    leftovers <= 1
}

#[cfg(test)]
mod tests {
    use super::is_smalltalk;

    #[test]
    fn greetings_thanks_and_farewells_are_small_talk() {
        for msg in [
            "Halo",
            "hai!",
            "Hi",
            "Hello 👋",
            "halo Lex",
            "Halo, apa kabar?",
            "selamat pagi",
            "Selamat pagi Bu Aira",
            "selamat siang kak",
            "assalamualaikum",
            "Terima kasih",
            "terima kasih ya",
            "makasih banyak",
            "thanks",
            "Thank you!",
            "thx",
            "good morning",
            "how are you?",
            "sampai jumpa",
            "bye",
            "see you later",
            "sampai nanti",
            "dadah",
        ] {
            assert!(is_smalltalk(msg), "should be small talk: {msg:?}");
        }
    }

    #[test]
    fn confirmations_are_never_small_talk() {
        // They continue a pending action; skipping recall there would hurt.
        for msg in [
            "ya", "Ya", "oke", "ok", "lanjut", "siap", "baik", "boleh", "kirim", "no", "yes",
        ] {
            assert!(
                !is_smalltalk(msg),
                "confirmation misread as small talk: {msg:?}"
            );
        }
    }

    #[test]
    fn a_request_next_to_a_greeting_is_a_request() {
        for msg in [
            "halo, tolong kirim email ke Alvin",
            "hai cek inbox",
            "selamat pagi, ada email baru?",
            "terima kasih, sekarang balas Alvin",
            "thanks, now send it",
            "hi can you show my leads",
            "halo status task hari ini",
            "makasih, buat laporan ya",
        ] {
            assert!(!is_smalltalk(msg), "request misread as small talk: {msg:?}");
        }
    }

    #[test]
    fn data_in_the_message_disqualifies_it() {
        for msg in [
            "halo 123",
            "hi https://example.com",
            "halo @alvin",
            "hai a@b.co",
            "halo /reset",
        ] {
            assert!(!is_smalltalk(msg), "message carrying data: {msg:?}");
        }
    }

    #[test]
    fn long_or_empty_or_unrelated_messages_are_not_small_talk() {
        assert!(!is_smalltalk(""));
        assert!(!is_smalltalk("   "));
        assert!(!is_smalltalk("👍"));
        assert!(!is_smalltalk(
            "cuaca hari ini bagus sekali dan langit cerah"
        ));
        assert!(
            !is_smalltalk("halo halo halo halo halo halo halo"),
            "over the word limit"
        );
        assert!(!is_smalltalk("Toko Melati Sejahtera"));
    }

    #[test]
    fn at_most_one_addressee_word_is_tolerated() {
        assert!(is_smalltalk("halo aira"));
        assert!(
            !is_smalltalk("halo aira lex toko"),
            "several unknown words look like content"
        );
    }
}
