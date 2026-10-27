# hdrneon

glxgears, but HDR. some neon tubes on a dark wall, and a strip of test patches along the bottom.

    hdrneon                          # hdr10
    hdrneon --mode scrgb
    hdrneon --mode sdr --cycle 5     # sdr -> hdr10 -> scrgb every 5s

`hdrneon --help` for the rest. m or space switches mode, q quits.

needs vulkan 1.3. the hdr modes only work if your display/compositor is actually in hdr, otherwise it'll tell you it
can't find a format. on linux that means wayland, with a compositor and mesa new enough to do hdr.

binaries are on the releases page. on macos you also need `brew install vulkan-loader molten-vk` (new enough for
vulkan 1.3), and `xattr -d com.apple.quarantine hdrneon` since it isn't signed. tested on linux (radv). mac and windows
build, but haven't had much use yet.

## the strip

- top: grey at 0, 1, 10, 100, 203, 400, 1000, 4000 nits
- middle: bt.709 vs bt.2020 red, green, blue, same brightness in each pair. the 2020 one should look more saturated.
  if they look the same, something's clipping to 709
- bottom: 0-1000 nit ramp. banding means something is dropping bits

in hdr10 the top row is 0 153 307 520 594 668 769 923 (10-bit full range), or 64 195 327 509 573 636 723 855 limited.

## building

    cargo build --release

the shaders are prebuilt in `shaders/`. if you change one, run `shaders/build.sh` (needs glslc).

GPL-3.0
