{
  buildCommit,
  lib,
  stdenv,
  fetchurl,
  python3,
  cmake,
  gnumake,
  pkg-config,
  patchelf,
  perl,
  bison,
  flex,
  which,
}:
let
  sources = (builtins.fromJSON (builtins.readFile ../third_party/voice/sources.json)).sources;
  archives = map (
    source:
    fetchurl {
      name = source.archive;
      inherit (source) url sha256;
    }
  ) sources;
  target = "x86_64-unknown-linux-gnu";
in
assert stdenv.hostPlatform.system == "x86_64-linux";
stdenv.mkDerivation {
  pname = "codex-voice-runtime";
  version = "1.28.6";
  src = lib.cleanSource ../third_party/voice;
  outputs = [
    "out"
    "dev"
  ];
  nativeBuildInputs = [
    python3
    cmake
    gnumake
    pkg-config
    patchelf
    perl
    bison
    flex
    which
  ];
  dontConfigure = true;
  # Upstream's preparer validates relative runpaths. Generic Nix fixups must
  # not replace them with SDK/build paths after the receipt is generated.
  dontFixup = true;
  buildPhase = ''
    runHook preBuild
    mkdir archives
    ${lib.concatMapStringsSep "\n" (archive: "cp ${archive} archives/${archive.name}") archives}
    python3 - <<'PY'
    import os
    import subprocess
    from pathlib import Path
    from types import SimpleNamespace
    import build_native

    prepare_sources = build_native.prepare_sources
    def prepare_nix_sources(*args):
        prepare_sources(*args)
        subprocess.run(
            ["${stdenv.shell}", "-c", 'source "${stdenv}/setup"; patchShebangs "$1"',
             "patch-voice-shebangs", str(args[1])], check=True,
        )
    build_native.prepare_sources = prepare_nix_sources

    args = SimpleNamespace(
        target="${target}", deployment_target=None, archives=Path("archives"),
        output=Path("native-build"), jobs=int(os.environ.get("NIX_BUILD_CORES", "1")),
        cc=Path("${stdenv.cc}/bin/cc"), cxx=Path("${stdenv.cc}/bin/c++"),
        cmake=Path("${cmake}/bin/cmake"), make=Path("${gnumake}/bin/make"),
        pkg_config=Path("${pkg-config}/bin/pkg-config"), shell=Path("${stdenv.shell}"),
        ar=Path("${stdenv.cc.bintools}/bin/ar"), ranlib=Path("${stdenv.cc.bintools}/bin/ranlib"),
        bootstrap_make=None, windows_build_inputs=None,
        c_flag=[], cxx_flag=[], link_flag=[],
    )
    builder = build_native.NativeBuild(args, os.environ)
    run_step = builder.run
    def run_nix_step(name, command, **kwargs):
        if name == "libffi-configure":
            # Nix's GCC reports ../lib64 even with an explicit --libdir.
            command = [*command, f"--libdir={builder.prefix}/lib", "--disable-multi-os-directory"]
        run_step(name, command, **kwargs)
        if name.endswith("-install") and builder.prefix.exists():
            subprocess.run(
                ["${stdenv.shell}", "-c", 'source "${stdenv}/setup"; patchShebangs "$1"',
                 "patch-voice-installed-tools", str(builder.prefix)], check=True,
            )
    builder.run = run_nix_step
    # The standalone recipe assumes /usr/bin. A Nix sandbox instead supplies
    # declared tools and compiler-wrapper settings through its build environment.
    builder.environment["PATH"] = os.environ["PATH"]
    builder.environment.update({k: v for k, v in os.environ.items() if k.startswith("NIX_")})
    try:
        builder.build()
    except Exception:
        if builder.record["steps"]:
            log = builder.output / (builder.record["steps"][-1]["name"] + ".log")
            if log.is_file():
                print(log.read_text(errors="replace"))
        raise
    # The CMake/Autoconf metadata also needs to relocate into the exported SDK.
    for metadata in (builder.prefix / "lib/pkgconfig").glob("*.pc"):
        metadata.write_text(metadata.read_text().replace(
            str(builder.prefix), "''${pcfiledir}/../..",
        ))
    PY
    # Compiler wrappers may add Nix store runpaths. All non-system media
    # dependencies are projected together by the upstream closure inspector.
    while IFS= read -r -d $'\0' library; do
      patchelf --set-rpath '$ORIGIN:$ORIGIN/..' "$library"
    done < <(find native-build/prefix -regextype posix-extended -type f -regex '.*\.so(\.[0-9]+)*' -print0)
    runHook postBuild
  '';
  installPhase = ''
    runHook preInstall
    python3 - <<'PY'
    import os
    from pathlib import Path
    from prepare_built_runtime import prepare_built

    status = Path("status.txt")
    status.write_text("STABLE_GIT_COMMIT ${buildCommit}\n")
    prepare_built(
        Path("native-build/prefix"), Path("native-build/built.json"), status,
        "${target}", Path(os.environ["out"]), sdk_output=Path(os.environ["dev"]),
    )
    PY
    runHook postInstall
  '';
  meta.platforms = [ "x86_64-linux" ];
}
