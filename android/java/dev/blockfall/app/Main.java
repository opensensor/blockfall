package dev.blockfall.app;

import android.app.NativeActivity;
import android.os.Build;
import android.os.Bundle;
import android.view.View;
import android.view.Window;
import android.view.WindowInsets;
import android.view.WindowInsetsController;
import android.view.WindowManager;

/**
 * NativeActivity that owns immersive-fullscreen enforcement for the game.
 *
 * The native (winit/Bevy) side cannot reliably poke the Java window from its
 * own thread, so all system-bar policy lives here: the status bar, the nav
 * pill and the display cutout are hidden/re-asserted on create, focus regain
 * and resume (bars otherwise reappear after every app switch or pull-down).
 * Sticky-immersive means a swipe shows the bars as ghosts that auto-hide
 * again without ever stealing layout from the playfield.
 *
 * Compiled by scripts/build-android.sh (javac + d8) into classes.dex.
 */
public class Main extends NativeActivity {
    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        applyImmersive();
    }

    @Override
    protected void onResume() {
        super.onResume();
        applyImmersive();
    }

    @Override
    public void onWindowFocusChanged(boolean hasFocus) {
        super.onWindowFocusChanged(hasFocus);
        if (hasFocus) {
            applyImmersive();
        }
    }

    private void applyImmersive() {
        Window window = getWindow();
        WindowManager.LayoutParams attrs = window.getAttributes();
        attrs.layoutInDisplayCutoutMode =
                WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_SHORT_EDGES;
        window.setAttributes(attrs);
        window.setStatusBarColor(0x00000000);
        window.setNavigationBarColor(0x00000000);
        if (Build.VERSION.SDK_INT >= 28) {
            window.setNavigationBarDividerColor(0x00000000);
        }
        if (Build.VERSION.SDK_INT >= 30) {
            window.setDecorFitsSystemWindows(false);
            WindowInsetsController controller = window.getInsetsController();
            if (controller != null) {
                controller.hide(WindowInsets.Type.systemBars());
                controller.setSystemBarsBehavior(
                        WindowInsetsController.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE);
            }
        }
        // Legacy sticky-immersive path: the only mechanism on API 26-29 and
        // still honored by apps targeting SDK < 35.
        int ui = View.SYSTEM_UI_FLAG_LAYOUT_STABLE
                | View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION
                | View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN
                | View.SYSTEM_UI_FLAG_HIDE_NAVIGATION
                | View.SYSTEM_UI_FLAG_FULLSCREEN
                | View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY;
        window.getDecorView().setSystemUiVisibility(ui);
    }
}
