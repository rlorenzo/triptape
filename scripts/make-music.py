import math, struct, wave

SR = 48000
DUR = 96.0  # seconds, loopable (16 bars x 6 s; the 4-chord cycle runs 4 times)
N = int(SR * DUR)

# Chord progression Am - F - C - G (A minor), one chord per 6 s bar
chords = [
    [110.00, 164.81, 261.63, 329.63],   # Am
    [87.31, 174.61, 261.63, 349.23],    # F
    [130.81, 196.00, 261.63, 392.00],   # C
    [98.00, 196.00, 293.66, 392.00],    # G
]
# pentatonic melody notes (A minor pentatonic)
mel = [440.0, 523.25, 659.25, 783.99, 880.0, 659.25, 523.25, 392.0]

out = [0.0] * N
HARM2 = 0.3  # 2nd-harmonic level

def add_sine(freq, start, dur, gain, decay=0.0):
    s = int(start * SR); e = min(N, int((start + dur) * SR))
    for i in range(s, e):
        t = (i - s) / SR
        env = math.exp(-decay * t) if decay else (1.0 if t < dur - 0.5 else max(0.0, (dur - t) / 0.5))
        ph = 2 * math.pi * freq * t
        out[i] += gain * env * (math.sin(ph) + HARM2 * math.sin(2 * ph) + 0.08 * math.sin(3 * ph))

bar = 6.0
for b in range(16):
    ch = chords[b % 4]
    t0 = b * bar
    # pad
    for f in ch:
        add_sine(f / 2, t0, bar + 0.3, 0.05)
        add_sine(f, t0, bar + 0.3, 0.035)
    # bass root
    add_sine(ch[0] / 2, t0, bar, 0.10, decay=0.25)
    # arpeggio plucks: 8ths
    for k in range(12):
        f = ch[(k * 2 + b) % len(ch)] * 2
        add_sine(f, t0 + k * 0.5, 0.45, 0.055, decay=6.0)
    # melody: one soft note per bar, pentatonic walk
    add_sine(mel[b % len(mel)], t0 + 1.0, 1.8, 0.045, decay=1.8)
    add_sine(mel[(b + 3) % len(mel)], t0 + 3.5, 1.4, 0.035, decay=2.0)

# global fade in/out 2s + normalize to 0.7 peak
peak = max(abs(x) for x in out) or 1.0
fade = int(2 * SR)
frames = bytearray()
g0 = 0.7 / peak
for i, x in enumerate(out):
    g = g0
    if i < fade: g *= i / fade
    if i > N - fade: g *= (N - i) / fade
    v = max(-1.0, min(1.0, x * g))
    frames += struct.pack('<hh', int(v * 32767), int(v * 32767))

with wave.open('/tmp/triptape-theme.wav', 'wb') as w:
    w.setnchannels(2); w.setsampwidth(2); w.setframerate(SR)
    w.writeframes(bytes(frames))
print("wrote /tmp/triptape-theme.wav peak", peak)
