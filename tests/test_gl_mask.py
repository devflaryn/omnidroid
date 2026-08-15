"""The renderer mask: what the game is told its GPU is.

    python3 -m pytest tests/test_gl_mask.py -q

The guest renders on llvmpipe (software) or virgl (host GPU), and answers
`glGetString(GL_RENDERER)` with a string no phone has ever reported. The mask
puts Mesa's own override variables into the GAME's environment via the
`wrap.<package>` property Zygote reads at fork time.

Two things here are not style points:

  * an Android property value is 91 characters, and an over-long `setprop`
    FAILS rather than truncating -- while still exiting 0. A boot that
    silently kept its real renderer would look identical to a masked one.
  * the property proves nothing on its own. Zygote refuses `wrap.` for an app
    it does not consider debuggable, silently, so the only evidence is the
    running process's own /proc/<pid>/environ.
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from omnidroid import glmask  # noqa: E402


def test_the_default_value_fits_an_android_property():
    value = glmask.wrap_value()
    assert glmask.value_fits(value), f"{len(value)} > {glmask.PROP_VALUE_MAX}"


def test_an_over_long_value_is_reported_not_truncated():
    value = glmask.wrap_value("x" * 200, "y" * 200)
    # The caller must refuse this. setprop exits 0 on a value the property
    # service rejects, so a truncate-and-hope here would leave the boot
    # unmasked with a success message.
    assert not glmask.value_fits(value)


def test_values_are_quoted_for_the_shell_zygote_runs_them_in():
    # "Adreno (TM) 650" contains parentheses and spaces, and the whole
    # property value is re-parsed by `sh -c` when the app is exec'd. Unquoted,
    # the app would not start at all.
    value = glmask.wrap_value("Adreno (TM) 650", "Qualcomm")
    assert "='Adreno (TM) 650'" in value
    assert "='Qualcomm'" in value


def test_a_quote_in_the_name_cannot_break_out():
    value = glmask.wrap_value("Ad'reno", None)
    assert value == "MESA_GL_RENDERER_OVERRIDE='Ad'\\''reno'"


def test_apply_script_reads_the_property_back():
    value = glmask.wrap_value()
    script = glmask.build_apply_script(value)
    assert "setprop wrap.com.roblox.client" in script
    # The read-back is the whole point: setprop's exit code does not tell you
    # whether the property service accepted the value.
    assert "getprop wrap.com.roblox.client" in script
    assert glmask.parse_applied(f"WRAP:{value}") == value


def test_apply_script_quotes_the_readback():
    # Unquoted, `echo WRAP:$(getprop …)` word-splits the spaces out of the
    # value, and the caller's equality check then fails on a mask that worked.
    assert '"WRAP:$(getprop' in glmask.build_apply_script("X=1")


def test_verify_distinguishes_not_applied_from_cannot_tell():
    # These need opposite responses: "the game does not have it" is a bug to
    # report, "the game is not running yet" is not.
    assert glmask.parse_verify("ENV:1")[0] is True
    assert glmask.parse_verify("ENV:0")[0] is False
    assert glmask.parse_verify("ENV:NO_PID")[0] is None
    assert glmask.parse_verify("")[0] is None


def test_the_tells_are_recognised():
    assert glmask.looks_like_a_tell("llvmpipe (LLVM 15.0.7, 256 bits)")
    assert glmask.looks_like_a_tell("virgl (NVIDIA GeForce RTX 4060/PCIe/SSE2)")
    assert glmask.looks_like_a_tell("ANGLE (NVIDIA, SwiftShader Device)")
    assert not glmask.looks_like_a_tell("Adreno (TM) 650")
    assert not glmask.looks_like_a_tell(None)


def test_renderer_line_is_pulled_off_dumpsys():
    dump = ("GLES: Mesa, llvmpipe, OpenGL ES 3.2 Mesa 23.1.9\n"
            "EGL implementation : 1.5\n")
    line = glmask.renderer_from_dumpsys(dump)
    assert line.startswith("GLES: Mesa, llvmpipe")
    assert glmask.looks_like_a_tell(line)


def test_the_mask_is_off_unless_asked_for():
    # OFF by default is a MEASUREMENT, not caution: the wrapped launch is
    # killed by seccomp on the x86 base (see the module docstring). A future
    # change that flips this default has to change this test, deliberately.
    from omnidroid import engine
    assert engine.gl_mask_settings({}) == (None, None)
    assert engine.gl_mask_settings(
        {"hiding": {"gl_renderer": "Mali-G78"}}) == (None, None)


def test_turning_it_on_takes_the_configured_strings():
    from omnidroid import engine
    r, v = engine.gl_mask_settings(
        {"hiding": {"gl_mask": "on", "gl_renderer": "Mali-G78"}})
    assert r == "Mali-G78" and v == glmask.DEFAULT_VENDOR
    assert engine.gl_mask_settings(
        {"hiding": {"gl_mask": "on"}}) == (glmask.DEFAULT_RENDERER,
                                           glmask.DEFAULT_VENDOR)
