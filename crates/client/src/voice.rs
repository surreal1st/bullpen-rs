//! Client text-to-speech - port of `projects/bullpen-night/src/client/voice.ts`
//! (`canSpeak`, `listVoices`, `speak`, `stopSpeaking`; mic input stays elsewhere).

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;
#[cfg(target_arch = "wasm32")]
use web_sys::SpeechSynthesisUtterance;

pub fn can_speak() -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window().is_some_and(|w| w.speech_synthesis().is_ok())
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        false
    }
}

#[cfg(target_arch = "wasm32")]
pub fn list_voice_names() -> Vec<String> {
    if !can_speak() {
        return Vec::new();
    }
    let synth = web_sys::window().unwrap().speech_synthesis().unwrap();
    synth
        .get_voices()
        .iter()
        .filter_map(|v| {
            v.dyn_ref::<web_sys::SpeechSynthesisVoice>()
                .map(|voice| voice.name())
        })
        .collect()
}

#[cfg(not(target_arch = "wasm32"))]
pub fn list_voice_names() -> Vec<String> {
    Vec::new()
}

#[cfg(target_arch = "wasm32")]
pub fn subscribe_voices(mut cb: impl FnMut() + 'static) {
    cb();
    let closure = Closure::new(move || cb());
    let window = web_sys::window().unwrap();
    let synth = window.speech_synthesis().unwrap();
    synth
        .add_event_listener_with_callback("voiceschanged", closure.as_ref().unchecked_ref())
        .ok();
    closure.forget();
}

#[cfg(not(target_arch = "wasm32"))]
pub fn subscribe_voices(_cb: impl FnMut() + 'static) {}

#[cfg(target_arch = "wasm32")]
fn strip_markdown(text: &str) -> String {
    let mut s = text.to_string();
    while let Some(start) = s.find("```") {
        if let Some(rest) = s[start + 3..].find("```") {
            s.replace_range(start..start + 3 + rest + 3, " ");
        } else {
            break;
        }
    }
    while let Some(start) = s.find('`') {
        if let Some(end) = s[start + 1..].find('`') {
            let inner = s[start + 1..start + 1 + end].to_string();
            s.replace_range(start..start + 1 + end + 1, &inner);
        } else {
            break;
        }
    }
    s = s.replace("![", "");
    while let Some(open) = s.find('[') {
        if let Some(mid) = s[open..].find("](") {
            let label = s[open + 1..open + mid].to_string();
            if let Some(close) = s[open + mid + 2..].find(')') {
                s.replace_range(open..open + mid + 2 + close + 1, &label);
            } else {
                break;
            }
        } else {
            break;
        }
    }
    s.lines()
        .map(|line| {
            let mut l = line.trim_start();
            while l.starts_with('#') {
                l = l.trim_start_matches('#').trim_start();
            }
            l.strip_prefix('>').unwrap_or(l).trim_start()
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

#[cfg(target_arch = "wasm32")]
fn find_voice(name: Option<&str>) -> Option<web_sys::SpeechSynthesisVoice> {
    let name = name.filter(|n| !n.is_empty())?;
    web_sys::window()?
        .speech_synthesis()
        .ok()?
        .get_voices()
        .iter()
        .filter_map(|v| v.dyn_ref::<web_sys::SpeechSynthesisVoice>().cloned())
        .find(|v| v.name() == name)
}

pub fn speak(text: &str, on_end: Option<Box<dyn FnOnce()>>, voice_name: Option<&str>) {
    #[cfg(target_arch = "wasm32")]
    {
        if !can_speak() {
            if let Some(done) = on_end {
                done();
            }
            return;
        }
        let clean = strip_markdown(text);
        if clean.is_empty() {
            if let Some(done) = on_end {
                done();
            }
            return;
        }
        let synth = web_sys::window().unwrap().speech_synthesis().unwrap();
        synth.cancel();
        let utterance = SpeechSynthesisUtterance::new_with_text(&clean).unwrap();
        if let Some(voice) = find_voice(voice_name) {
            utterance.set_voice(Some(&voice));
        }
        if let Some(done) = on_end {
            let end = Closure::once(Box::new(move || done()) as Box<dyn FnOnce()>);
            let cb = end.as_ref().unchecked_ref();
            utterance.set_onend(Some(cb));
            utterance.set_onerror(Some(cb));
            end.forget();
        }
        synth.speak(&utterance);
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        if let Some(done) = on_end {
            done();
        }
        let _ = (text, voice_name);
    }
}

pub fn stop_speaking() {
    #[cfg(target_arch = "wasm32")]
    if let Some(window) = web_sys::window() {
        if let Ok(synth) = window.speech_synthesis() {
            synth.cancel();
        }
    }
}
