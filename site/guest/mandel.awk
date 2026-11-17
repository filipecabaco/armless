# One CPU's share of a Mandelbrot frame: rows J, J+P, J+2P... of the top half, each written to its
# place on the screen with its mirror image in the bottom half. Points in the main cardioid and the
# period-2 bulb are not iterated. The shading follows the log of the escape time.
BEGIN {
  s = " .:-=+*#%@"; L = log(m + 1); h = int((H + 1) / 2)
  for (y = J; y < h; y += P) {
    ci = 1.1 * (2 * y / (H - 1) - 1); c2 = ci * ci; l = ""
    for (x = 0; x < W; x++) {
      cr = -0.6 + 1.7 * (2 * x / W - 1); p = cr - 0.25; q = p * p + c2
      if (q * (q + p) < 0.25 * c2 || (cr + 1) * (cr + 1) + c2 < 0.0625) i = m
      else { a = 0; b = 0; for (i = 0; i < m; i++) { u = a * a; v = b * b; if (u + v > 4) break; b = 2 * a * b + ci; a = u - v + cr } }
      l = l substr(s, int(9 * log(i + 1) / L) + 1, 1)
    }
    printf "\033[%d;1H%s\033[%d;1H%s", y + 1, l, H - y, l
  }
}
