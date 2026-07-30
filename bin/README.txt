gitwatch-x86_64-linux-glibc : prebuilt release binary (dynamically linked to
glibc, interpreter /lib64/ld-linux-x86-64.so.2).

It will NOT run as-is on NixOS (no FHS loader at that path). On NixOS, build
from source instead:
    nix build        # -> ./result/bin/gitwatch
    # or
    cargo build --release
On a standard FHS distro (Ubuntu/Fedora/Arch...) the binary runs directly.
