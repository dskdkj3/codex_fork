{
  sourcePackage,
  lib,
  fetchurl,
  python3,
  autoPatchelfHook,
  openssl,
  libcap,
  ncurses,
  llvmPackages,
  stdenv,
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
in
assert stdenv.hostPlatform.system == "x86_64-linux";
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
  ];
  buildInputs = (old.buildInputs or [ ]) ++ [
    openssl
    libcap
    ncurses
    stdenv.cc.cc.libgcc
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
  };
  cargoBuildFlags = [
    "--bin"
    "codex"
    "--bin"
    "codex-code-mode-host"
    "--bin"
    "bwrap"
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
  passthru = (old.passthru or { }) // {
    supportsLocalContext = true;
    supportsLocalContextSubagents = true;
    canonicalPackageLayout = 1;
  };
})
