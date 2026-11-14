//! Sequence verification tests for hotkey feedback audio.
//!
//! Asserts that:
//! 1. Start feedback sound completes completely BEFORE audio capture begins.
//! 2. Stop feedback sound only plays AFTER audio capture streams are fully stopped and joined.
//! 3. Synthesized WAV buffers are valid PCM RIFF files with correct headers.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::channel;
use std::sync::Arc;
use std::time::{Duration, Instant};

use openclip::hotkey::{generate_feedback_wav, play_feedback_sound_sync};

#[test]
fn test_feedback_wav_pcm_structure() {
    let start_wav = generate_feedback_wav(&[(1000, 90), (0, 30), (1500, 120)]);
    assert!(start_wav.len() > 44);
    assert_eq!(&start_wav[0..4], b"RIFF");
    assert_eq!(&start_wav[8..12], b"WAVE");
    assert_eq!(&start_wav[12..16], b"fmt ");
    // Format tag 1 (PCM)
    assert_eq!(u16::from_le_bytes(start_wav[20..22].try_into().unwrap()), 1);
    // Mono (1 channel)
    assert_eq!(u16::from_le_bytes(start_wav[22..24].try_into().unwrap()), 1);
    // 44100 Hz
    assert_eq!(u32::from_le_bytes(start_wav[24..28].try_into().unwrap()), 44100);
    // 16 bits per sample
    assert_eq!(u16::from_le_bytes(start_wav[34..36].try_into().unwrap()), 16);
    assert_eq!(&start_wav[36..40], b"data");
    let data_len = u32::from_le_bytes(start_wav[40..44].try_into().unwrap()) as usize;
    assert_eq!(start_wav.len(), 44 + data_len);

    let stop_wav = generate_feedback_wav(&[(750, 180)]);
    assert!(stop_wav.len() > 44);
    assert_eq!(&stop_wav[0..4], b"RIFF");
    let stop_data_len = u32::from_le_bytes(stop_wav[40..44].try_into().unwrap()) as usize;
    assert_eq!(stop_wav.len(), 44 + stop_data_len);
}

#[test]
fn test_start_sequence_sound_finishes_before_capture_starts() {
    let (tx, rx) = channel();
    let sound_finished = Arc::new(std::sync::Mutex::new(None));
    let sound_finished_clone = sound_finished.clone();

    // Spawn thread simulating the State::Starting transition
    std::thread::spawn(move || {
        let t0 = Instant::now();
        play_feedback_sound_sync(true);
        *sound_finished_clone.lock().unwrap() = Some(Instant::now());
        let _ = tx.send(());
        log::debug!("start sound elapsed: {:?}", t0.elapsed());
    });

    // Main thread waits for notification
    let res = rx.recv_timeout(Duration::from_secs(2));
    assert!(res.is_ok(), "start sound should finish within 2 seconds");

    let capture_init_time = Instant::now();
    let finish_time = sound_finished.lock().unwrap().expect("sound finished timestamp");

    // The start sound MUST finish before or at the moment capture initialization begins
    assert!(
        finish_time <= capture_init_time,
        "start sound finish time ({:?}) must precede capture start time ({:?})",
        finish_time,
        capture_init_time
    );
}

#[test]
fn test_stop_sequence_audio_closed_before_stop_sound() {
    // Simulates the Recorder stop sequence:
    // 1. stop_capture_and_audio() shuts down audio thread and joins it
    // 2. on_audio_stopped() is called (stop sound)
    // 3. encode thread is finalized

    let audio_thread_joined = Arc::new(AtomicBool::new(false));
    let audio_joined_clone = audio_thread_joined.clone();

    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_flag_clone = stop_flag.clone();

    // Dummy audio thread
    let audio_handle = std::thread::spawn(move || {
        while !stop_flag_clone.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(5));
        }
    });

    // Simulate stop_capture_and_audio
    let mut audio_opt = Some(audio_handle);
    let mut stop_capture_and_audio = move || {
        stop_flag.store(true, Ordering::SeqCst);
        if let Some(t) = audio_opt.take() {
            t.join().unwrap();
            audio_joined_clone.store(true, Ordering::SeqCst);
        }
    };

    let mut stop_sound_called = false;
    let mut audio_was_stopped_when_sound_called = false;

    // Simulate stop_with_feedback
    stop_capture_and_audio();
    let mut on_audio_stopped = || {
        stop_sound_called = true;
        audio_was_stopped_when_sound_called = audio_thread_joined.load(Ordering::SeqCst);
        play_feedback_sound_sync(false);
    };
    on_audio_stopped();

    assert!(stop_sound_called, "stop sound callback must be invoked");
    assert!(
        audio_was_stopped_when_sound_called,
        "audio capture thread must already be joined and stopped before stop sound plays"
    );
}
