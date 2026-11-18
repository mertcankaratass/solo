"""OmniShip pipeline for Solo: Check -> Build -> Ship.

Pull requests and pushes to main run the CI workflow (build.yml), which runs
Check and then Build. A pushed v* tag runs the Release workflow (ship.yml):
Check, Build, then a GitHub release carrying the lean native bindings for
every target and the macOS menu helpers.

Run `omniship generate` after editing this file and commit the regenerated
files with it.
"""

import shutil
import subprocess

from omniship import Pipeline, Platform
from omniship.plugins.bun import BunToolchain
from omniship.plugins.github import (
    GitHubActions,
    GitHubActionStep,
    GitHubMatrix,
    GitHubPullRequest,
    GitHubPush,
    GitHubPushedTag,
    GitHubRelease,
    GitHubRunner,
    GitHubWorkflow,
)
from omniship.plugins.system import SystemPackages

REPOSITORY = "octacity-org/solo"
NATIVE = "packages/native"

# Release targets: Rust target -> runner, artifact platform, napi file suffix.
# The x86_64 macOS and arm64 Windows bindings are cross-compiled.
TARGETS = {
    "aarch64-apple-darwin": (GitHubRunner.MACOS_LATEST, Platform.MACOS_ARM64, "darwin-arm64"),
    "x86_64-apple-darwin": (GitHubRunner.MACOS_LATEST, Platform.MACOS_X86_64, "darwin-x64"),
    "x86_64-unknown-linux-gnu": (GitHubRunner.UBUNTU_LATEST, Platform.LINUX_X86_64, "linux-x64-gnu"),
    "aarch64-unknown-linux-gnu": (GitHubRunner.UBUNTU_24_04_ARM, Platform.LINUX_ARM64, "linux-arm64-gnu"),
    "x86_64-pc-windows-msvc": (GitHubRunner.WINDOWS_LATEST, Platform.WINDOWS_X86_64, "win32-x64-msvc"),
    "aarch64-pc-windows-msvc": (GitHubRunner.WINDOWS_LATEST, Platform.WINDOWS_ARM64, "win32-arm64-msvc"),
}  # fmt: skip

github = GitHubActions(
    default_runner=GitHubRunner.UBUNTU_LATEST,
    # Only called by build.yml, so a pull request runs Check once.
    check=GitHubWorkflow(file="check.yml", name="Check", triggers=[]),
    build=GitHubWorkflow(
        file="build.yml",
        name="CI",
        triggers=[
            GitHubPush(
                branches=["main"],
                paths_ignore=["**/*.md", "LICENSE", "**/*.gitignore", "docs/**"],
            ),
            GitHubPullRequest(),
        ],
        cancel_superseded=True,
    ),
    ship=GitHubWorkflow(
        file="ship.yml",
        name="Release",
        triggers=[GitHubPush(tags=["v*"])],
    ),
)
pipeline = Pipeline(targets=[github])

BUN = BunToolchain(version="1.4.4")
# The toolchain comes from rust-toolchain.toml. Empty rustflags and
# build-warnings keep the action from setting RUSTFLAGS="-D warnings" and
# CARGO_BUILD_WARNINGS=deny; lint policy stays in the crate.
RUST_INPUTS = {"cache": False, "rustflags": "", "build-warnings": ""}
SETUP_RUST = GitHubActionStep("setup-rust-toolchain", "Set up Rust", RUST_INPUTS)
INSTALL_NEXTEST = GitHubActionStep(
    "taiki-e/install-action@75a7c0f87bd666af3cdb75c5b9e9d03ab6044e6f",  # v2.90.6
    "Install cargo-nextest",
    {"tool": "nextest"},
)
# GPUI's Linux build and display dependencies (Vulkan, Wayland, X11, fonts),
# based on longbridge/gpui-component and Zed's script/linux.
LINUX_BUILD_PACKAGES = [
    "gcc", "g++", "clang", "libfontconfig-dev", "libwayland-dev",
    "libxkbcommon-x11-dev", "libx11-xcb-dev", "libssl-dev", "libzstd-dev",
    "libvulkan1", "libwebkit2gtk-4.1-dev", "libjavascriptcoregtk-4.1-dev",
]  # fmt: skip
ENV = {"MACOSX_DEPLOYMENT_TARGET": "10.13"}
# Xvfb has no DRI3 and runners have no GPU, so Vulkan must use Mesa's
# lavapipe CPU driver.
LAVAPIPE = {
    "VK_DRIVER_FILES": "/usr/share/vulkan/icd.d/lvp_icd.x86_64.json",
    "LIBGL_ALWAYS_SOFTWARE": "1",
}


def cargo_cache(name, restore_prefix):
    return github.cache(
        paths=["~/.cargo/registry", "~/.cargo/git", f"{NATIVE}/target/"],
        key=[name, "cargo", github.hash_files(f"{NATIVE}/Cargo.lock")],
        restore_prefixes=[restore_prefix],
    )


def run(ctx, *arguments, cwd=".", env=None):
    """Run a command from `cwd` (relative to the repository) and stream it."""
    process_env = {**ctx.env, **(env or {})}
    path = process_env.get("PATH")
    executable = shutil.which(arguments[0], path=path) or arguments[0]
    process = subprocess.Popen(
        [executable, *arguments[1:]],
        cwd=ctx.workspace / cwd,
        env=process_env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        encoding="utf-8",
        errors="replace",
    )
    for line in process.stdout:
        ctx.log.output(line.rstrip("\r\n"))
    if process.wait() != 0:
        ctx.fail(f"{' '.join(arguments)} failed")


def restore_generated_bindings(ctx):
    """napi rewrites index.js/index.d.ts for the host; the checked-in files
    are the macOS declarations the TypeScript builds expect."""
    run(ctx, "git", "checkout", "--", f"{NATIVE}/index.js", f"{NATIVE}/index.d.ts")


@pipeline.check
def check(stage):
    @stage.task(
        requires=[
            BUN,
            SystemPackages(
                ubuntu=[*LINUX_BUILD_PACKAGES, "mesa-vulkan-drivers", "libegl1",
                        "libegl-mesa0", "xvfb"],
            ),  # fmt: skip
        ],
        execution=github.job(
            runners=[GitHubRunner.UBUNTU_LATEST],
            env={**ENV, **LAVAPIPE},
            caches=[cargo_cache("linux-test", "linux-test-cargo-")],
            before_steps=[SETUP_RUST],
        ),
    )
    def linux_test(ctx):
        run(ctx, "bun", "install")
        # napi runs `cargo metadata` without --locked before building, which
        # silently rewrites a stale Cargo.lock, so check the lock first.
        run(ctx, "cargo", "metadata", "--locked", "--format-version", "1", cwd=NATIVE)

        # The tasks suite drives a live app, which needs the automation
        # feature that release bindings leave out.
        run(ctx, "bun", "run", "build:release:full", cwd=NATIVE)
        restore_generated_bindings(ctx)
        run(ctx, "bun", "run", "build", cwd="packages/core")
        run(ctx, "bun", "run", "build", cwd="packages/solid")
        run(ctx, "bun", "run", "build:automation", cwd="examples/tasks")
        run(
            ctx, "xvfb-run", "-a", "--server-args=-screen 0 1280x1024x24",
            "bun", "run", "test", cwd="examples/tasks",
            env={"SOLO_SCREENSHOT_DIR": str(ctx.workspace / "examples/tasks/screenshots")},
        )  # fmt: skip

        # The Solid native suites mount GPUI's headless TestPlatform windows
        # (no X server), which need the test-support binding.
        run(ctx, "bun", "run", "build:debug", cwd=NATIVE)
        restore_generated_bindings(ctx)
        run(ctx, "bun", "run", "test", cwd="packages/solid")

    @stage.task(
        requires=[BUN],
        execution=github.job(
            runners=[GitHubRunner.MACOS_LATEST],
            env=ENV,
            caches=[cargo_cache("macos-test", "macos-test-cargo-")],
            before_steps=[SETUP_RUST, INSTALL_NEXTEST],
        ),
    )
    def macos_test(ctx):
        # gpui_apple compiles shaders with `metal`, which Xcode 26 no longer
        # ships by default ("missing Metal Toolchain").
        run(ctx, "xcodebuild", "-downloadComponent", "MetalToolchain")
        run(ctx, "bun", "install")
        # Default features include test-support (TestGpuixRenderer); this also
        # builds the solo-context-menu helper.
        run(ctx, "bun", "run", "build", "--target", "aarch64-apple-darwin", cwd=NATIVE)
        run(ctx, "bun", "run", "test", cwd="packages/core")
        run(
            ctx, "cargo", "nextest", "run", "--locked",
            "--manifest-path", f"{NATIVE}/context-menu-host/Cargo.toml",
        )  # fmt: skip
        run(ctx, "bun", "run", "build", cwd="packages/core")
        run(
            ctx, "cargo", "nextest", "run", "--locked", "--features", "test-support",
            "--lib", "--test", "layout_probe", cwd=NATIVE,
        )  # fmt: skip
        run(ctx, "bun", "run", "test", cwd="packages/solid")
        run(ctx, "bun", "run", "build", cwd="packages/solid")
        # The fixtures import @solo/solid's built types, so this follows the build.
        run(ctx, "bun", "run", "typecheck", cwd="packages/solid")
        run(ctx, "bunx", "tsc", "-p", "examples")
        run(ctx, "bun", "run", "build:automation", cwd="examples/tasks")
        run(ctx, "bun", "run", "test", cwd="examples/tasks")


@pipeline.build
def build(stage):
    @stage.task(
        requires=[BUN, SystemPackages(ubuntu=[*LINUX_BUILD_PACKAGES, "libgtk-3-dev"])],
        execution=github.job(
            runners=list(dict.fromkeys(runner for runner, _, _ in TARGETS.values())),
            # Two targets share a runner, so `include` alone would merge them
            # into one job; pair every runner with every target and exclude
            # the pairs that are not a target's own runner.
            matrix=GitHubMatrix(
                axes={"target": list(TARGETS)},
                exclude=[
                    {"runner": runner.value, "target": target}
                    for runner in dict.fromkeys(r for r, _, _ in TARGETS.values())
                    for target, (own, _, _) in TARGETS.items()
                    if own != runner
                ],
            ),
            env={**ENV, "SOLO_TARGET": github.matrix("target")},
            caches=[cargo_cache(github.matrix("target"), "${{ matrix.target }}-cargo-")],
            before_steps=[
                GitHubActionStep(
                    "setup-rust-toolchain",
                    "Set up Rust",
                    {**RUST_INPUTS, "target": "${{ matrix.target }}"},
                )
            ],
        ),
    )
    def bindings(ctx):
        """Build one target's lean (no default features) binding, plus the
        menu helper on macOS. These files are the release assets."""
        target = ctx.env.get("SOLO_TARGET")
        if target not in TARGETS:
            ctx.fail(f"SOLO_TARGET must name a release target, got {target!r}")
        _, platform, suffix = TARGETS[target]
        darwin = target.endswith("-apple-darwin")
        if darwin:
            run(ctx, "xcodebuild", "-downloadComponent", "MetalToolchain")
        run(ctx, "bun", "install")
        run(ctx, "bun", "run", "build:release", "--target", target, cwd=NATIVE)
        ctx.artifacts.add(f"{NATIVE}/solo-native.{suffix}.node", platform=platform)
        if darwin:
            helper = f"solo-context-menu.{suffix}"
            ctx.artifacts.add(f"{NATIVE}/{helper}", platform=platform)


@pipeline.ship
def ship(stage):
    # Publishes the pushed tag with every Build artifact; the title defaults
    # to the tag and the notes are generated by GitHub.
    stage.task(
        GitHubRelease(repository=REPOSITORY, tag=GitHubPushedTag(), prerelease="auto"),
        execution=github.job(timeout_minutes=10),
    )
