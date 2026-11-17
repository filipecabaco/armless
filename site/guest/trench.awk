# One CPU's share of a trench run frame: rows J, J+P, J+2P... of the screen, each written to its
# place. The trench is ray cast: every character is a ray from the cockpit that hits the floor, a wall
# or the sky, and its brightness falls with the distance. Nothing is stored between frames, so every
# CPU draws the same picture from the run's seed (SEED) and the frame number (f).
#
# The seed decides how the cockpit weaves, which wall panels are windows, vents or turrets, and when
# and where the TIE fighters and their bolts show up. The run ends (frame F) in the exhaust port.
function hash(a, b,   v) {
  v = sin(a * 12.9898 + b * 78.233 + seed * 0.0013) * 43758.5453
  if (v < 0) v = -v
  return v - int(v)
}
function cam(zz) { return amp * (0.7 * sin(0.17 * zz + ph1) + 0.4 * sin(0.05 * zz + ph2)) }
function put(x, y, c, k) {
  x = x < 0 ? -1 : int(x + 0.5); y = y < 0 ? -1 : int(y + 0.5)
  if (x >= 0 && x < W && y >= 0 && y < H) { ov[y, x] = c; oc[y, x] = k }
}
function lerp(a, b, p) { return a + (b - a) * p }
function shade(b, t,   i) {
  i = int(b * 1.6 / (1 + 0.35 * t) * 9.99)
  return substr(ramp, (i > 9 ? 9 : i) + 1, 1)
}
BEGIN {
  seed = SEED + 0
  ramp = " .:-=+*#%@"
  A = 1.5; B = 0.8; Rim = 1.4; V = 0.5
  Fo = W / 2; cx = W / 2; cy = H * 0.45
  z = f * V
  fh = F - 8                       # the frame the torpedo goes in
  Zp = fh * V + 7                  # the exhaust port, down the trench
  amp = (fh - f) / 20; if (amp > 1) amp = 1; if (amp < 0) amp = 0
  ph1 = hash(1, 1) * 6.28; ph2 = hash(2, 2) * 6.28
  x0 = cam(z)

  put(cx - 2, cy, "[", ""); put(cx, cy, "+", ""); put(cx + 2, cy, "]", "")

  # TIE fighters come down the trench, some of them firing, never in the same place twice
  for (k = 0; k < 3; k++) {
    off = k * 13 + int(hash(k, 9) * 20)
    ph = (f + off) % 38; c = int((f + off) / 38) * 3 + k
    d = 34 - 0.9 * ph
    if (d < 2) continue
    px = cx + ((hash(c, 11) - 0.5) * 2 * (A - 0.25) - x0) * Fo / d
    py = cy + (-0.15 - hash(c, 12)) * Fo / (2 * d)
    if (hash(c, 13) < 0.6) {
      q = (ph % 8) / 8
      put(lerp(px, px + (px - cx) * 3, q), lerp(py, py + (py - cy) * 3, q), "*", 92)
      put(lerp(px, px + (px - cx) * 3, q - 0.12), lerp(py, py + (py - cy) * 3, q - 0.12), ".", 92)
    }
    w = int(Fo * 0.25 / d) + 1; hh = int(w / 2); b = int(w / 3)
    for (yy = -hh; yy <= hh; yy++) { put(px - w, py + yy, "|", ""); put(px + w, py + yy, "|", "") }
    for (xx = -w; xx <= w; xx++) if (xx < -b || xx > b) put(px + xx, py, "-", "")
    for (yy = -int(b / 2); yy <= int(b / 2); yy++) for (xx = -b; xx <= b; xx++) put(px + xx, py + yy, "@", "")
    put(px, py, "O", "")
  }

  # Our own bolts, from the bottom corners to where we are headed, until the torpedo goes
  if (f < fh - 14 && hash(int(f / 9), 21) < 0.7) {
    p = (f % 9 + 1) / 9
    for (s = 0; s < 2; s++) {
      sx = s ? W - 1 : 0
      put(lerp(sx, cx, p), lerp(H - 1, cy, p), "*", 91)
      put(lerp(sx, cx, p - 0.12), lerp(H - 1, cy, p - 0.12), "+", 91)
    }
  }

  # The port, and the proton torpedo on its way to it
  tP = Zp - z
  if (tP > 0.5) { pxp = cx - x0 * Fo / tP; pyp = cy + B * Fo / (2 * tP) }
  if (f >= fh - 14 && f < fh) {
    p = (f - (fh - 14) + 1) / 14
    for (s = 0; s < 3; s++) put(lerp(cx, pxp, p - s * 0.05), lerp(H - 1, pyp, p - s * 0.05), s ? "." : "O", 96)
  }
  if (f >= fh) {                   # the port was at the centre of the screen
    pxp = cx; pyp = cy + 2
    R = (f - fh + 1) * Fo * 0.3
  }

  for (y = J; y < H; y += P) {
    dy = (y - cy) * 2 / Fo; l = ""
    for (x = 0; x < W; x++) {
      c = " "; k = ""
      if (f >= fh && (d = sqrt((x - pxp) ^ 2 + (2 * (y - pyp)) ^ 2)) < R && hash(x + 0.5, y + f * 31) > d / R * 0.8) {
        r = d / R + hash(y + 0.5, x + f * 17) * 0.4
        c = substr("@@%#*+:. ", int(r * 7) + 1, 1); k = r < 0.45 ? 97 : r < 0.8 ? 93 : 91
      } else if ((y, x) in ov) { c = ov[y, x]; k = oc[y, x] }
      else {
        dx = (x - cx) / Fo
        tw = 1e9; if (dx > 0) tw = (A - x0) / dx; else if (dx < 0) tw = (A + x0) / -dx
        tf = 1e9; if (dy > 0) tf = B / dy
        if (tf < tw) {             # the floor: panels, pipes, vents and the port
          zz = z + tf; xx = x0 + dx * tf; u = zz % 2; g = (xx + 10) % 0.75
          hk = hash(int(zz / 2), 5)
          b = 0.35
          if (u < 0.15 || g < 0.08) b = 0.8
          else if (hk < 0.25 && xx > 0.2 && xx < 0.9 && u > 0.5 && u < 1.5) b = 0.12
          if (xx > -0.4 && xx < 0.4 && zz > Zp - 0.35 && zz < Zp + 0.35) {
            if (xx > -0.3 && xx < 0.3 && zz > Zp - 0.25 && zz < Zp + 0.25) b = 0
            else { b = 1; k = 96 }
          }
          c = shade(b, tf)
        } else if (tw < 1e9 && dy * tw >= -Rim) {   # a wall
          zz = z + tw; yw = dy * tw; u = zz % 2; hk = hash(int(zz / 2), (dx > 0) + 3)
          b = 0.5
          if ((yw + 5) % 0.45 < 0.06) b = 0.85
          if (u < 0.08) b = 0.9
          if (hk < 0.3 && yw > -0.6 && yw < 0.2 && u > 0.4 && u < 1.6) b = 0.18
          else if (hk > 0.9 && yw > -1 && yw < 0.3 && u > 0.3 && u < 1.7) { b = 1; k = 91 }
          else if (hk > 0.3 && hk < 0.42 && yw > -0.5 && yw < -0.3 && u > 0.9 && u < 1.1) { b = 1; k = 93 }
          c = shade(b, tw)
        } else {                   # the sky
          h = hash(x * 3.1, y * 7.7)
          if (h > 0.97) c = h > 0.995 ? "+" : "."
        }
      }
      l = l (k == "" ? c : "\033[" k "m" c "\033[m")
    }
    # One write per row: a frame is several KB, and when the CPUs' output is split anywhere else a
    # row lands where another CPU just put the cursor
    printf "\033[%d;1H%s", y + 1, l
    fflush()
  }
}
