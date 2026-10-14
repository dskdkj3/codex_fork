{
  sourcePackage,
  buildCommit,
  callPackage,
  lib,
  fetchurl,
  python3,
  autoPatchelfHook,
  openssl,
  libcap,
  ncurses,
  llvmPackages,
  stdenv,
  alsa-lib,
  patchelf,
}:
let
  target = "x86_64-unknown-linux-gnu";
  v8Version = "150.4.0";
  v8Base = "https://github.com/openai/codex/releases/download/rusty-v8-v${v8Version}";
  v8Archive = fetchurl {
    url = "${v8Base}/librusty_v8_ptrcomp_sandbox_release_${target}.a.gz";
    sha256 = "a35c75d1f26e6a983885a45b33490a4ebe54f05050568b32b89cfb421b30b583";
  };
  v8Binding = fetchurl {
    url = "${v8Base}/src_binding_ptrcomp_sandbox_release_${target}.rs";
    sha256 = "7727826ae479bdb645e807239fb12d1f8e2e23de7a6cf16f5ee592690d1d8506";
  };
  # Reuse the exact upstream DotSlash resource identities without executing
  # DotSlash or fetching mutable resources from inside the build sandbox.
  readManifest =
    path: builtins.fromJSON (lib.removePrefix "#!/usr/bin/env dotslash\n\n" (builtins.readFile path));
  zshSpec = (readManifest ../scripts/codex_package/codex-zsh).platforms.linux-x86_64;
  rgSpec = (readManifest ../scripts/codex_package/rg).platforms.linux-x86_64;
  zshArchive = fetchurl {
    url = (builtins.head zshSpec.providers).url;
    sha256 = zshSpec.digest;
  };
  rgArchive = fetchurl {
    url = (builtins.head rgSpec.providers).url;
    sha256 = rgSpec.digest;
  };
  lock = builtins.fromTOML (builtins.readFile ../codex-rs/Cargo.lock);
  lockedV8 = builtins.filter (package: package.name == "v8") lock.package;
  voiceRuntime = callPackage ./voice-runtime.nix { inherit buildCommit; };
in
assert stdenv.hostPlatform.system == "x86_64-linux";
assert buildCommit != null && builtins.match "[0-9a-f]{40}" buildCommit != null;
assert builtins.length lockedV8 == 1 && (builtins.head lockedV8).version == v8Version;
sourcePackage.overrideAttrs (old: {
  # Keep package scripts and all compile-time resources beside the Rust tree.
  src = lib.cleanSource ../.;
  postUnpack = ''
    sourceRoot="$sourceRoot/codex-rs"
  '';
  nativeBuildInputs = (old.nativeBuildInputs or [ ]) ++ [
    python3
    autoPatchelfHook
    patchelf
  ];
  buildInputs = (old.buildInputs or [ ]) ++ [
    openssl
    libcap
    ncurses
    stdenv.cc.cc.libgcc
    alsa-lib
    voiceRuntime.dev
  ];
  env = (old.env or { }) // {
    RUSTY_V8_ARCHIVE = v8Archive;
    RUSTY_V8_SRC_BINDING_PATH = v8Binding;
    LIBCLANG_PATH = "${llvmPackages.libclang.lib}/lib";
    CC = "${llvmPackages.clang}/bin/clang";
    CXX = "${llvmPackages.clang}/bin/clang++";
    # cc-rs prefers these lowercase target keys over the cargo hook's HOST_CC/HOST_CXX.
    CC_x86_64_unknown_linux_gnu = "${llvmPackages.clang}/bin/clang";
    CXX_x86_64_unknown_linux_gnu = "${llvmPackages.clang}/bin/clang++";
    # aws-lc-sys 0.45 probes HOST_CC before cc-rs reads the target-specific
    # compiler. Keep its probe on Clang despite cargoBuildHook's GCC HOST_CC,
    # or it passes GNU-only assembler flags to Clang's integrated assembler.
    AWS_LC_SYS_HOST_CC = "${llvmPackages.clang}/bin/clang";
    AWS_LC_SYS_HOST_CXX = "${llvmPackages.clang}/bin/clang++";
    STABLE_GIT_COMMIT = buildCommit;
    CODEX_BUILD_COMMIT = buildCommit;
    PKG_CONFIG_PATH = lib.concatStringsSep ":" [
      "${voiceRuntime.dev}/lib/pkgconfig"
      "${alsa-lib.dev}/lib/pkgconfig"
      (old.env.PKG_CONFIG_PATH or "")
    ];
  };
  cargoBuildFlags = [
    "--bin"
    "codex"
    "--bin"
    "codex-code-mode-host"
    "--bin"
    "bwrap"
    "--bin"
    "codex-voice-host"
  ];
  installPhase = ''
    runHook preInstall
    mkdir -p "$TMPDIR/codex-zsh-resource" "$TMPDIR/codex-rg-resource"
    tar -xzf ${zshArchive} -C "$TMPDIR/codex-zsh-resource"
    tar -xzf ${rgArchive} -C "$TMPDIR/codex-rg-resource"
    CODEX_REPO_ROOT="$PWD/.." ${python3}/bin/python3 ../scripts/build_codex_package.py \
      --target ${target} \
      --package-version ${lib.escapeShellArg old.version} \
      --package-dir "$out" \
      --entrypoint-bin "target/${target}/release/codex" \
      --code-mode-host-bin "target/${target}/release/codex-code-mode-host" \
      --bwrap-bin "target/${target}/release/bwrap" \
      --zsh-bin "$TMPDIR/codex-zsh-resource/${zshSpec.path}" \
      --rg-bin "$TMPDIR/codex-rg-resource/${rgSpec.path}"
    runHook postInstall
  '';
  # Assemble after the normal ELF fixups/stripping so upstream's receipts hash
  # the final files. Keep media libraries physical and package-relative: the
  # helper rejects symlinked runtime files and never scans system plugins.
  # postFixup itself runs before dependency postFixupHooks. Register our
  # assembler last during preFixup, after autoPatchelfHook's registration.
  preFixup = (old.preFixup or "") + ''
    postFixupHooks+=(assembleCodexVoice)
    assembleCodexVoice() {
    voice_helper="$TMPDIR/codex-voice-host"
    install -m755 "target/${target}/release/codex-voice-host" "$voice_helper"
    strip --strip-debug "$voice_helper"
    patchelf --set-rpath '$ORIGIN/../lib:${
      lib.makeLibraryPath [
        alsa-lib
        stdenv.cc.cc.libgcc
        stdenv.cc.libc
      ]
    }' "$voice_helper"
    python3 ../third_party/voice/release_runtime.py stage \
      --target ${target} --source ${voiceRuntime} --output "$TMPDIR/voice-runtime"
    python3 ../third_party/voice/release_runtime.py seal \
      --target ${target} --output "$TMPDIR/voice-runtime"
    python3 ../third_party/voice/assemble_package.py \
      --package "$out" --helper "$voice_helper" --voice-target ${target} \
      --build-commit ${lib.escapeShellArg buildCommit} \
      --release-version ${lib.escapeShellArg old.version} \
      --runtime "$TMPDIR/voice-runtime" --output "$TMPDIR/codex-with-voice"
    cp -a "$TMPDIR/codex-with-voice/codex-resources/voice" "$out/codex-resources/voice"
    python3 ../nix/check-voice-package.py "$out"
    }
  '';
  passthru = (old.passthru or { }) // {
    supportsLocalContext = true;
    supportsModelContextOverrides = true;
    supportsModelContextPresets = true;
    supportsResourceGuard = true;
    canonicalPackageLayout = 1;
    supportsVoice = true;
    inherit voiceRuntime buildCommit;
  };
})
