#!/usr/bin/env python3
"""Generate placeholder SFX/BGM WAVs for T18 (44.1 kHz mono 16-bit PCM).

Sine/noise blips with exponential envelopes; the BGM is a 2.000 s loop whose
every note has an integer number of periods so the seam is click-free.
Re-run with `python3 generate.py` from the assets directory.
"""

import math
import os
import random
import struct
import wave

SR = 44100
PEAK = 0.55


def frame(value):
    value = max(-1.0, min(1.0, value))
    return struct.pack("<h", int(value * 32767 * PEAK))


def write(name, samples):
    path = os.path.join(os.path.dirname(os.path.abspath(__file__)), name)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with wave.open(path, "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(SR)
        w.writeframes(b"".join(frame(s) for s in samples))
    print(f"{name}: {len(samples) / SR:.3f} s, {os.path.getsize(path)} bytes")


def tone(freq, dur, decay=18.0, vib=0.0):
    n = int(SR * dur)
    out = []
    for i in range(n):
        t = i / SR
        f = freq * (1.0 + vib * math.sin(2 * math.pi * 6.0 * t))
        env = math.exp(-decay * t) * min(1.0, t / 0.003)
        out.append(math.sin(2 * math.pi * f * t) * env)
    return out


def sweep(f0, f1, dur, decay=6.0):
    n = int(SR * dur)
    out = []
    phase = 0.0
    for i in range(n):
        t = i / SR
        f = f0 + (f1 - f0) * t / dur
        phase += 2 * math.pi * f / SR
        env = math.exp(-decay * t)
        out.append(math.sin(phase) * env)
    return out


def noise(dur, decay=30.0, lp=0.25):
    n = int(SR * dur)
    out = []
    prev = 0.0
    for i in range(n):
        t = i / SR
        prev = prev + lp * (random.uniform(-1.0, 1.0) - prev)
        out.append(prev * math.exp(-decay * t))
    return out


def note(freq, dur):
    """Whole-period tone (freq rounded so `dur` holds an integer cycle count)."""
    cycles = max(1, round(freq * dur))
    n = int(SR * dur)
    f = cycles / dur
    out = []
    for i in range(n):
        t = i / SR
        env = min(1.0, t / 0.004) * (1.0 - 0.35 * t / dur)
        out.append(math.sin(2 * math.pi * f * t) * env)
    return out


def main():
    random.seed(18)
    write("sfx/move.wav", tone(660, 0.06, 40.0))
    write("sfx/rotate.wav", tone(880, 0.05, 45.0))
    write("sfx/lock.wav", tone(196, 0.09, 30.0))
    write("sfx/line-clear.wav", tone(440, 0.07, 25.0) + tone(880, 0.08, 25.0))
    arp = 0.065
    write(
        "sfx/tetris.wav",
        tone(523, arp, 12.0)
        + tone(659, arp, 12.0)
        + tone(784, arp, 12.0)
        + tone(1046, 0.09, 10.0),
    )
    write("sfx/tspin.wav", tone(330, 0.22, 8.0, vib=0.25))
    write("sfx/level-up.wav", sweep(440, 1760, 0.25, 5.0))
    write("sfx/hard-drop.wav", noise(0.07, 40.0, 0.35))
    write("sfx/hold.wav", tone(523, 0.07, 35.0))
    write("sfx/game-over.wav", sweep(440, 110, 0.30, 4.0))

    # BGM: 8 eighth notes of 0.25 s at 120 BPM = exactly 2.000 s.
    bass = [110, 110, 165, 110, 147, 147, 110, 98]
    lead = [440, 523, 659, 523, 587, 494, 440, 392]
    bars = []
    for b, l in zip(bass, lead):
        bar = note(b, 0.25)
        half = note(l, 0.125)
        mel = half + [0.0] * (len(bar) - len(half))
        bars.append([0.75 * bar[i] + 0.30 * mel[i] for i in range(len(bar))])
    write("bgm_loop.wav", [s for bar in bars for s in bar])


if __name__ == "__main__":
    main()
