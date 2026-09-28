package com.omnidroid.probe;

import android.app.Activity;
import android.content.Context;
import android.content.res.Configuration;
import android.graphics.Canvas;
import android.os.Bundle;
import android.util.Log;
import android.view.KeyEvent;
import android.view.MotionEvent;
import android.view.View;

/**
 * The launcher Activity: says that it was created, then fills its window with one colour. It
 * handles a display resize itself (the manifest's configChanges) and says so: the configuration it
 * was given, the size its view was laid out at, and the size it then drew at. And it says what
 * input reaches it: each key (its key code, scan code and source) and each pointer event (action,
 * source, tool, position on the screen, buttons, scroll). C asks for the pointer capture and R
 * releases it, as a game's camera lock does; it says when it holds the capture and what captured
 * motion reaches it (source and relative axes).
 */
public class MainActivity extends Activity {
    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        Log.i("OmniProbe", "onCreate " + getPackageName() + " pid " + android.os.Process.myPid());
        View v = new SizeView(this);
        v.setBackgroundColor(0xff2196f3);
        v.setFocusable(true);
        v.setFocusableInTouchMode(true);
        v.setOnCapturedPointerListener((view, e) -> {
            Log.i("OmniProbe", "captured " + MotionEvent.actionToString(e.getActionMasked()) + " source 0x" + Integer.toHexString(e.getSource())
                    + " rel " + Math.round(e.getAxisValue(MotionEvent.AXIS_RELATIVE_X)) + "," + Math.round(e.getAxisValue(MotionEvent.AXIS_RELATIVE_Y))
                    + " buttons " + e.getButtonState());
            return true;
        });
        content = v;
        setContentView(v);
        v.requestFocus();
        // Its first frame: drawn by the app, then committed to SurfaceFlinger.
        v.getViewTreeObserver().registerFrameCommitCallback(() -> Log.i("OmniProbe", "frame committed"));
    }

    @Override
    protected void onResume() {
        super.onResume();
        Log.i("OmniProbe", "onResume");
    }

    @Override
    public void onConfigurationChanged(Configuration c) {
        super.onConfigurationChanged(c);
        Log.i("OmniProbe", "onConfigurationChanged screen " + c.screenWidthDp + "x" + c.screenHeightDp + " dp, smallest "
                + c.smallestScreenWidthDp + " dp, density " + c.densityDpi + ", keyboard " + c.keyboard);
    }

    private View content;

    @Override
    public void onPointerCaptureChanged(boolean hasCapture) {
        Log.i("OmniProbe", "pointer capture " + (hasCapture ? "on" : "off"));
    }

    @Override
    public boolean dispatchKeyEvent(KeyEvent e) {
        if (e.getAction() == KeyEvent.ACTION_DOWN && e.getKeyCode() == KeyEvent.KEYCODE_C) {
            content.requestPointerCapture();
        } else if (e.getAction() == KeyEvent.ACTION_DOWN && e.getKeyCode() == KeyEvent.KEYCODE_R) {
            content.releasePointerCapture();
        }
        Log.i("OmniProbe", "key " + (e.getAction() == KeyEvent.ACTION_DOWN ? "down" : "up") + " code " + e.getKeyCode()
                + " scan " + e.getScanCode() + " repeat " + e.getRepeatCount() + " source 0x" + Integer.toHexString(e.getSource()));
        return super.dispatchKeyEvent(e);
    }

    @Override
    public boolean dispatchTouchEvent(MotionEvent e) {
        logMotion(e);
        return super.dispatchTouchEvent(e);
    }

    @Override
    public boolean dispatchGenericMotionEvent(MotionEvent e) {
        logMotion(e);
        return super.dispatchGenericMotionEvent(e);
    }

    private static void logMotion(MotionEvent e) {
        Log.i("OmniProbe", "motion " + MotionEvent.actionToString(e.getActionMasked()) + " source 0x" + Integer.toHexString(e.getSource())
                + " tool " + e.getToolType(0) + " at " + Math.round(e.getRawX()) + "," + Math.round(e.getRawY())
                + " buttons " + e.getButtonState() + " vscroll " + e.getAxisValue(MotionEvent.AXIS_VSCROLL));
    }

    /** The content view: logs each size it is laid out at, and the first draw at each. */
    static final class SizeView extends View {
        private int drawnW = -1, drawnH = -1;

        SizeView(Context context) {
            super(context);
        }

        @Override
        protected void onSizeChanged(int w, int h, int oldw, int oldh) {
            super.onSizeChanged(w, h, oldw, oldh);
            Log.i("OmniProbe", "view size " + w + "x" + h + " (was " + oldw + "x" + oldh + ")");
        }

        @Override
        protected void onDraw(Canvas canvas) {
            super.onDraw(canvas);
            if (getWidth() != drawnW || getHeight() != drawnH) {
                drawnW = getWidth();
                drawnH = getHeight();
                Log.i("OmniProbe", "drawn " + drawnW + "x" + drawnH);
            }
        }
    }
}
