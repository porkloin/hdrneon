# hdrneon

Like glxgears, but for HDR.

<p align="center"><img src="screenshot.png" alt="hdrneon running" width="540"></p>

    hdrneon                          # hdr10
    hdrneon --mode scrgb
    hdrneon --mode sdr --cycle 5     # sdr -> hdr10 -> scrgb every 5s

`hdrneon --help` for the rest. M or space switches mode, Q quits.

Only tested on Linux (RADV). The macOS and Windows builds compile, but haven't been tested at all.

## Prereqs

- Vulkan 1.3
- For the HDR modes, a display and compositor that are actually in HDR. On Linux that means Wayland, with a compositor
  and Mesa new enough to do HDR.
- On macOS: `brew install vulkan-loader molten-vk`

## Installing

On Linux, grab the release tarball, check it against its checksum, and put the binary in `~/.local/bin`:

    name=hdrneon-$(uname -m)-unknown-linux-gnu
    curl -fLO https://github.com/porkloin/hdrneon/releases/latest/download/$name.tar.gz
    curl -fLO https://github.com/porkloin/hdrneon/releases/latest/download/$name.tar.gz.sha256
    sha256sum -c $name.tar.gz.sha256
    tar xzf $name.tar.gz
    install -Dm755 $name/hdrneon ~/.local/bin/hdrneon

Or with Homebrew:

    brew install porkloin/tap/hdrneon

Works with Homebrew on macOS and Linux.

Linux Homebrew note: on older Linux distros where Homebrew uses its own glibc, it may not be able to load your GPU
driver. In that case, use a binary from the [releases page](https://github.com/porkloin/hdrneon/releases) instead.

macOS release binary note: run `xattr -d com.apple.quarantine hdrneon` first, since the binary isn't signed.

## Analyzing output with the test strip

- Top: grey at 0, 1, 10, 100, 203, 400, 1000, 4000 nits.
- Middle: BT.709 vs BT.2020 red, green, blue, same brightness in each pair. The 2020 one should look more saturated.
  If they look the same, something's clipping to 709.
- Bottom: a 0-1000 nit ramp. Banding means something is dropping bits.

In HDR10 the top row's patches should come out as these 10-bit values:

| nits | full range | limited range |
|-----:|-----------:|--------------:|
| 0    | 0          | 64            |
| 1    | 153        | 195           |
| 10   | 307        | 327           |
| 100  | 520        | 509           |
| 203  | 594        | 573           |
| 400  | 668        | 636           |
| 1000 | 769        | 723           |
| 4000 | 923        | 855           |

## Building

    cargo build --release

The shaders are prebuilt in `shaders/`. If you change one, run `shaders/build.sh` (needs glslc).

GPL-3.0
