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
 * source, tool, position on the screen, buttons, scroll).
 */
public class MainActivity extends Activity {
    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        Log.i("OmniProbe", "onCreate " + getPackageName() + " pid " + android.os.Process.myPid());
        View v = new SizeView(this);
        v.setBackgroundColor(0xff2196f3);
        setContentView(v);
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

    @Override
    public boolean dispatchKeyEvent(KeyEvent e) {
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
