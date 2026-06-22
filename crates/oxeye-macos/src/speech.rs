//! Speech output via the macOS **AVFoundation** `AVSpeechSynthesizer`.
//!
//! Mirrors the native-speech approach of the Windows (SAPI `ISpVoice`) and Linux
//! (speech-dispatcher) back-ends: a persistent synthesizer that speaks utterances asynchronously
//! and can **interrupt** in-progress speech (barge-in) when a higher-priority announcement
//! arrives — honoring [`oxeye_core::announcement::Announcement::interrupt`].
//!
//! `AVSpeechSynthesizer` is an Objective-C object; the `unsafe` message sends are confined here,
//! each with a `// SAFETY:` justification (enforced by clippy's `undocumented_unsafe_blocks`).

use objc2::rc::Retained;
use objc2_avf_audio::{AVSpeechBoundary, AVSpeechSynthesizer, AVSpeechUtterance};
use objc2_foundation::NSString;

/// A persistent macOS speech synthesizer.
///
/// Not `Send`/`Sync`: create and use it on a single thread (the polling loop's), as the other
/// back-ends do with their speech clients.
pub(crate) struct Speaker {
    synthesizer: Retained<AVSpeechSynthesizer>,
}

impl Speaker {
    /// Create a synthesizer that uses the system default voice.
    pub(crate) fn new() -> Self {
        // SAFETY: `+[AVSpeechSynthesizer new]` returns a new, owned (+1) synthesizer; `Retained`
        // releases it on drop.
        let synthesizer = unsafe { AVSpeechSynthesizer::new() };
        Self { synthesizer }
    }

    /// Speak `text`. When `interrupt` is set, stop any in-progress speech first (barge-in) so the
    /// newer announcement is not queued behind stale speech.
    pub(crate) fn speak(&self, text: &str, interrupt: bool) {
        if interrupt {
            // SAFETY: stop current speech immediately; the returned bool (whether anything was
            // speaking) is not needed.
            let _ = unsafe {
                self.synthesizer
                    .stopSpeakingAtBoundary(AVSpeechBoundary::Immediate)
            };
        }
        let string = NSString::from_str(text);
        // SAFETY: build an utterance from the owned `NSString` (AVFoundation copies it); returns
        // an owned (+1) utterance held by `Retained`.
        let utterance = unsafe { AVSpeechUtterance::speechUtteranceWithString(&string) };
        // SAFETY: enqueue the utterance for asynchronous synthesis on the synthesizer.
        unsafe { self.synthesizer.speakUtterance(&utterance) };
    }
}
