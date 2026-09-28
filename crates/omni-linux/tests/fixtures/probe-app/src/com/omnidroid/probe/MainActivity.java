package com.omnidroid.probe;

import android.app.Activity;
import android.content.Context;
import android.content.res.Configuration;
import android.graphics.Canvas;
import android.os.Bundle;
import android.util.Log;
import android.view.View;

/**
 * The launcher Activity: says that it was created, then fills its window with one colour. It
 * handles a display resize itself (the manifest's configChanges) and says so: the configuration it
 * was given, the size its view was laid out at, and the size it then drew at.
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
                + c.smallestScreenWidthDp + " dp, density " + c.densityDpi);
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
