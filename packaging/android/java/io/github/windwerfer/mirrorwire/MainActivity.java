package io.github.windwerfer.mirrorwire;

import android.app.NativeActivity;
import android.content.Context;
import android.content.Intent;
import android.media.MediaCodecInfo;
import android.media.MediaCodecList;
import android.net.Uri;
import android.net.wifi.WifiManager;
import android.os.Build;
import android.os.Bundle;
import android.view.WindowManager;
import java.io.File;
import java.io.FileOutputStream;
import java.nio.charset.StandardCharsets;

/**
 * Thin shell around the Rust viewer (libmirrorwire_android.so). It only does
 * what needs the Java API: hold the Wi-Fi multicast lock for mDNS, keep the
 * screen on, and hand mirrorwire:// links to Rust through a file in
 * getFilesDir() (= the native internalDataPath), which Rust polls. It also
 * lists the H.264 decoders (MediaCodecList has no NDK API) so Rust can pick a
 * hardware decoder by name.
 */
public class MainActivity extends NativeActivity {
    private WifiManager.MulticastLock multicastLock;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        writePendingUri(getIntent());
        writeDecoderList();
        super.onCreate(savedInstanceState);
        getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
        try {
            WifiManager wifi = (WifiManager) getApplicationContext().getSystemService(Context.WIFI_SERVICE);
            if (wifi != null) {
                multicastLock = wifi.createMulticastLock("mirrorwire-mdns");
                multicastLock.setReferenceCounted(false);
                multicastLock.acquire();
            }
        } catch (RuntimeException e) {
            android.util.Log.w("mirrorwire", "no multicast lock: " + e);
        }
    }

    @Override
    protected void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        setIntent(intent);
        writePendingUri(intent);
    }

    @Override
    protected void onDestroy() {
        if (multicastLock != null && multicastLock.isHeld()) {
            multicastLock.release();
        }
        super.onDestroy();
    }

    /** One line per video/avc decoder: name|hw|swOnly|lowLatency (1, 0 or ? when the API is too old). */
    private void writeDecoderList() {
        StringBuilder sb = new StringBuilder();
        try {
            for (MediaCodecInfo info : new MediaCodecList(MediaCodecList.REGULAR_CODECS).getCodecInfos()) {
                if (info.isEncoder()) continue;
                boolean avc = false;
                for (String type : info.getSupportedTypes()) {
                    if ("video/avc".equalsIgnoreCase(type)) avc = true;
                }
                if (!avc) continue;
                String hw = "?", sw = "?", lowLatency = "?";
                if (Build.VERSION.SDK_INT >= 29) {
                    hw = info.isHardwareAccelerated() ? "1" : "0";
                    sw = info.isSoftwareOnly() ? "1" : "0";
                }
                if (Build.VERSION.SDK_INT >= 30) {
                    lowLatency = info.getCapabilitiesForType("video/avc")
                            .isFeatureSupported(MediaCodecInfo.CodecCapabilities.FEATURE_LowLatency) ? "1" : "0";
                }
                sb.append(info.getName()).append('|').append(hw).append('|').append(sw)
                        .append('|').append(lowLatency).append('\n');
            }
        } catch (RuntimeException e) {
            android.util.Log.w("mirrorwire", "could not list decoders: " + e);
        }
        writeAtomically("h264_decoders.txt", sb.toString());
    }

    private void writeAtomically(String name, String text) {
        File tmp = new File(getFilesDir(), name + ".tmp");
        try (FileOutputStream out = new FileOutputStream(tmp)) {
            out.write(text.getBytes(StandardCharsets.UTF_8));
        } catch (java.io.IOException e) {
            android.util.Log.w("mirrorwire", "could not write " + name + ": " + e);
            return;
        }
        if (!tmp.renameTo(new File(getFilesDir(), name))) {
            android.util.Log.w("mirrorwire", "could not move " + name);
        }
    }

    private void writePendingUri(Intent intent) {
        if (intent == null) return;
        Uri data = intent.getData();
        if (data == null || !"mirrorwire".equals(data.getScheme())) return;
        File tmp = new File(getFilesDir(), "pending_uri.txt.tmp");
        try (FileOutputStream out = new FileOutputStream(tmp)) {
            out.write(data.toString().getBytes(StandardCharsets.UTF_8));
        } catch (java.io.IOException e) {
            android.util.Log.w("mirrorwire", "could not store link: " + e);
            return;
        }
        if (!tmp.renameTo(new File(getFilesDir(), "pending_uri.txt"))) {
            android.util.Log.w("mirrorwire", "could not move link file");
        }
    }
}
