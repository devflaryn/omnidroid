package com.omnidroid.probe;

import android.app.Activity;
import android.os.Bundle;
import android.util.Log;
import android.view.View;

/** The launcher Activity: says that it was created, then fills its window with one colour. */
public class MainActivity extends Activity {
    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        Log.i("OmniProbe", "onCreate " + getPackageName() + " pid " + android.os.Process.myPid());
        View v = new View(this);
        v.setBackgroundColor(0xff2196f3);
        setContentView(v);
    }

    @Override
    protected void onResume() {
        super.onResume();
        Log.i("OmniProbe", "onResume");
    }
}
