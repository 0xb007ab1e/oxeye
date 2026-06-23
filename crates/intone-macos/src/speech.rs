//! Speech output via the macOS **AVFoundation** `AVSpeechSynthesizer`.
//!
//! Mirrors the native-speech approach of the Windows (SAPI `ISpVoice`) and Linux
//! (speech-dispatcher) back-ends: a persistent synthesizer that speaks utterances asynchronously
//! and can **interrupt** in-progress speech (barge-in) when a higher-priority announcement
//! arrives — honoring [`intone_core::announcement::Announcement::interrupt`].
//!
//! Speech settings ([`intone_core::Speech`]) are applied per utterance: rate, volume, pitch, and
//! the synthesis voice (selected by identifier, then by language). Selecting a voice by *friendly
//! name* (enumerating `speechVoices`) is a follow-up that needs real hardware to verify.
//!
//! `AVSpeechSynthesizer` is an Objective-C object; the `unsafe` message sends are confined here,
//! each with a `// SAFETY:` justification (enforced by clippy's `undocumented_unsafe_blocks`).

use objc2::rc::Retained;
use objc2_avf_audio::{
    AVSpeechBoundary, AVSpeechSynthesisVoice, AVSpeechSynthesizer, AVSpeechUtterance,
    AVSpeechUtteranceDefaultSpeechRate, AVSpeechUtteranceMaximumSpeechRate,
    AVSpeechUtteranceMinimumSpeechRate,
};
use objc2_foundation::NSString;

use intone_core::Speech;

/// A persistent macOS speech synthesizer with the user's configured prosody and voice.
///
/// Not `Send`/`Sync`: create and use it on a single thread (the polling loop's), as the other
/// back-ends do with their speech clients.
pub(crate) struct Speaker {
    synthesizer: Retained<AVSpeechSynthesizer>,
    /// Selected voice, or `None` to use the system default.
    voice: Option<Retained<AVSpeechSynthesisVoice>>,
    /// AVFoundation speech rate (within `AVSpeechUtterance*SpeechRate`).
    rate: f32,
    /// Volume, 0.0–1.0.
    volume: f32,
    /// Pitch multiplier, 0.5–2.0 (1.0 = normal).
    pitch: f32,
}

impl Speaker {
    /// Create a synthesizer applying `speech`'s rate/volume/pitch and selected voice.
    pub(crate) fn new(speech: &Speech) -> Self {
        // SAFETY: `+[AVSpeechSynthesizer new]` returns a new, owned (+1) synthesizer; `Retained`
        // releases it on drop.
        let synthesizer = unsafe { AVSpeechSynthesizer::new() };
        Self {
            synthesizer,
            voice: resolve_voice(speech),
            rate: rate_to_av(speech.rate),
            volume: f32::from(speech.volume) / 100.0,
            pitch: pitch_multiplier(speech.pitch),
        }
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
        // SAFETY: set the utterance's prosody and (optional) voice before synthesis; all are
        // plain property setters on the owned utterance.
        unsafe {
            utterance.setRate(self.rate);
            utterance.setVolume(self.volume);
            utterance.setPitchMultiplier(self.pitch);
            if let Some(voice) = self.voice.as_deref() {
                utterance.setVoice(Some(voice));
            }
        }
        // SAFETY: enqueue the utterance for asynchronous synthesis on the synthesizer.
        unsafe { self.synthesizer.speakUtterance(&utterance) };
    }
}

/// Resolve the configured voice: by identifier first (e.g.
/// `com.apple.voice.compact.en-US.Samantha`), then by `speech.language`. `None` falls back to the
/// system default voice.
fn resolve_voice(speech: &Speech) -> Option<Retained<AVSpeechSynthesisVoice>> {
    if let Some(name) = speech.voice.as_deref() {
        let identifier = NSString::from_str(name);
        // SAFETY: look up a voice by identifier; returns `None` for an unknown or
        // not-yet-downloaded identifier.
        if let Some(voice) = unsafe { AVSpeechSynthesisVoice::voiceWithIdentifier(&identifier) } {
            return Some(voice);
        }
    }
    if let Some(language) = speech.language.as_deref() {
        let language = NSString::from_str(language);
        // SAFETY: look up the default voice for a BCP-47 language; `None` if unsupported.
        if let Some(voice) = unsafe { AVSpeechSynthesisVoice::voiceWithLanguage(Some(&language)) } {
            return Some(voice);
        }
    }
    None
}

/// Map a 0–100 rate (50 = normal) onto AVFoundation's speech-rate range: 50 at the engine
/// default, each half lerped toward the minimum / maximum.
fn rate_to_av(value: u8) -> f32 {
    // SAFETY: reading immutable AVFoundation `c_float` constants.
    let (min, default, max) = unsafe {
        (
            AVSpeechUtteranceMinimumSpeechRate,
            AVSpeechUtteranceDefaultSpeechRate,
            AVSpeechUtteranceMaximumSpeechRate,
        )
    };
    let v = f32::from(value);
    if v <= 50.0 {
        min + (default - min) * (v / 50.0)
    } else {
        default + (max - default) * ((v - 50.0) / 50.0)
    }
}

/// Map a 0–100 pitch (50 = normal) onto AVFoundation's 0.5–2.0 pitch-multiplier range.
fn pitch_multiplier(value: u8) -> f32 {
    let v = f32::from(value);
    if v <= 50.0 {
        0.5 + 0.5 * (v / 50.0)
    } else {
        1.0 + (v - 50.0) / 50.0
    }
}

#[cfg(test)]
mod tests {
    use super::pitch_multiplier;

    #[test]
    fn pitch_multiplier_centers_at_one() {
        assert!(
            (pitch_multiplier(50) - 1.0).abs() < f32::EPSILON,
            "50 = normal"
        );
        assert!(
            (pitch_multiplier(0) - 0.5).abs() < f32::EPSILON,
            "0 = lowest"
        );
        assert!(
            (pitch_multiplier(100) - 2.0).abs() < f32::EPSILON,
            "100 = highest"
        );
    }
}
