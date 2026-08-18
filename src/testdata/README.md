# Test fixtures

`mono16.jp2` and `rgb8.jp2` are the JPEG 2000 fixtures for `media::jp2`. Unlike
the TIFF/PNG fixtures (generated at test time by `src/testutil.rs`) these are
checked in: the tree has a JPEG 2000 *decoder* only, so there is nothing to
generate them with. They are a few hundred bytes each.

Both are **lossless** (reversible 5/3 wavelet), 64×32, and hold values a test
can state in closed form, so the decode is asserted bit-exactly:

* `mono16.jp2` — 16-bit grey, pixel *i* = `(i * 17) mod 65536`
* `rgb8.jp2` — 8-bit RGB, pixel *i* = `(i mod 256, 3i mod 256, 7i mod 256)`

They were produced with Pillow (which wraps OpenJPEG), i.e. by an independent
implementation of the format:

```python
from PIL import Image
w, h = 64, 32
im = Image.new("I;16", (w, h))
im.putdata([(i * 17) % 65536 for i in range(w * h)])
im.save("mono16.jp2", irreversible=False)

im = Image.new("RGB", (w, h))
im.putdata([(i % 256, (i * 3) % 256, (i * 7) % 256) for i in range(w * h)])
im.save("rgb8.jp2", irreversible=False)
```
