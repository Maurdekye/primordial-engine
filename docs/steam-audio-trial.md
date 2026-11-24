# Steam Audio friend-test trial

[I] The trial uses eight nearest binaural sources, a shared first-order Ambisonics bed, and 128-frame DSP blocks at 48 kHz. Measure the rendered workload and the user's streamed headset session before deciding on permanent adoption.

[V] The opt-in `steam-audio-trial` feature is for Windows x64 and Linux x64. Build both peer kits from the same commit. The runtime verifies the SHA-256 of Valve's unmodified v4.8.1 library before loading it. Keep the library and its redistribution notices in the generated kit outside Git. [Valve release](https://github.com/ValveSoftware/steam-audio/releases/tag/v4.8.1) · [Apache license](https://github.com/ValveSoftware/steam-audio/blob/v4.8.1/LICENSE.md)

| Artifact | SHA-256 |
| --- | --- |
| [V] SDK ZIP | `4a0aa5ec1176f38f0b0993a37c2259d9e86f27e22d5e24f83ec4c3cb9a1d5449` |
| [V] `steamaudio/lib/windows-x64/phonon.dll` | `ca3dbc01dbc24492717011e80f6a51404ca143ae344ca660971d2c983f1e058d` |
| [V] `steamaudio/lib/linux-x64/libphonon.so` | `faf7c4d69e0f104366f6553cda38d3f946f761f7db223baf12663f6fbf2387da` |

- [V] `PE_STEAM_AUDIO_LIB` enables the trial and points to the matching library. `PE_STEAM_AUDIO_LOG` gives the metrics file path. `PE_STEAM_AUDIO_DEVICE` selects an exact CPAL device name when the default is unsuitable. The app runs silently when the library fails verification or the endpoint cannot open.
- [V] The stream writes silence until explicitly launched with `PE_STEAM_AUDIO_AUDIBLE=1`. A silent fixture smoke uses `PE_STEAM_AUDIO_FIXTURE=32` and `pe-app --headless --seconds 8`. The fixture alternates wood and metal hits and tops up the mix with synthetic sources. Select 512 to exercise the full voice count. Only `--headless` uses `PE_STEAM_AUDIO_RENDER=1` to draw to a GPU texture without a window or pixel readback.
- [V] Linux output uses CPAL's ALSA backend. Build with the system ALSA development package available (`libasound2-dev` on Ubuntu or `alsa-lib-devel` on Fedora) and run with a 48 kHz system output route. `alsa-sys` links system libasound with checked-in Rust bindings. Run `python -B -I scripts/audit_steam_audio_trial.py` alongside the stock landing gate to check the enabled graph.

## Impact material and voice limits

| Presented shape | Trial timbre |
| --- | --- |
| [V] Box | Wood knock |
| [V] Sphere or capsule | Metal impact |
| [V] Mesh or missing shape | Wood knock |

[V] The same shape table applies on both builds. Procedural impacts use a short damped tone for wood and longer, inharmonic resonances for metal. Size scales pitch. Strength and distance set gain. The trial ranks hits by estimated playback level before admission, caps admission at 64 per presented display frame, and caps active voices at 512. Eight nearest voices use direct binaural processing and the rest share the bed. A body-pair match or cooldown suppresses duplicate impacts within 45 ms. Unmatched replay hits are eligible for 180 ms. Started voices decay naturally.

[I] Fixture results cover synthesis, spatial DSP, the callback and the presenter tap. Game-impact behavior also needs the published physics hit feed. In the headset trial, record near and distant single collisions and a grouped drop. Save each peer's Steam Audio metrics, app log, and OpenXR runtime and stream settings. Compare DSP and callback timing, active/dropped voices, stream errors and reported xruns alongside direction, loudness, latency and glitches reported by each listener. The log gives crowded blocks (8+ voices) their own DSP distribution so quiet intervals do not dilute it. The hit latency metric ends at the first carrying callback. Headset playback time and physical underruns require independent evidence.
