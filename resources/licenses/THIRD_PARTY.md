# Third-party programs bundled with TripTape

TripTape is MIT-licensed. It ships three separate programs that it runs as
child processes. They are not linked into TripTape, so their licences cover
those programs only.

## FFmpeg and ffprobe 9.0.2 (GPL version 3)

FFmpeg is free software licensed under the GNU General Public License,
version 3 (`GPL-3.0.txt` in this folder). The bundled builds are configured
with `--enable-gpl --enable-version3` and include GPL libraries such as
libx264 and libvidstab. They are not built with `--enable-nonfree`.

- macOS: the static build by Helmut K. C. Tessarek, <https://evermeet.cx/ffmpeg/>
  (`ffmpeg-9.0.2.zip`, `ffprobe-9.0.2.zip`). Apple silicon Macs run it
  through Rosetta 2.
- Windows: the "essentials" static build by Gyan Doshi,
  <https://www.gyan.dev/ffmpeg/builds/> (`ffmpeg-9.0.2-essentials_build.zip`).

Source code:

- FFmpeg 9.0.2: <https://ffmpeg.org/releases/ffmpeg-9.0.2.tar.xz>
  (the Windows build is commit
  <https://github.com/FFmpeg/FFmpeg/commit/946fcce07b>).
- The external libraries and their versions are listed by
  `ffmpeg -buildconf` and on the build pages above.

Pinned checksums and build configuration: `docs/ffmpeg-build.md` in the
TripTape source repository.

## Typst 0.15.1 (Apache License 2.0)

Typst is licensed under the Apache License, Version 2.0
(`Typst-LICENSE-Apache-2.0.txt`). Its notices for third-party components are
in `Typst-NOTICE.txt`. Source code: <https://github.com/typst/typst/tree/v0.15.1>.
