package org.mjsolex.mobile;

import android.app.NativeActivity;
import android.content.Intent;
import android.database.Cursor;
import android.net.Uri;
import android.os.Bundle;
import android.os.ParcelFileDescriptor;
import android.provider.OpenableColumns;

public final class MainActivity extends NativeActivity {
    static {
        System.loadLibrary("mjsolex");
    }

    private static final int OPEN_SER = 10;
    private static final int SAVE_PNG = 11;

    private native void onDocument(int request, int fd, String name, String error);

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        getWindow().setStatusBarColor(0xff11151b);
        getWindow().setNavigationBarColor(0xff11151b);
    }

    public void pickSer() {
        Intent intent = new Intent(Intent.ACTION_OPEN_DOCUMENT).setType("*/*");
        launchPicker(intent, OPEN_SER);
    }

    public void createPng(String name) {
        Intent intent = new Intent(Intent.ACTION_CREATE_DOCUMENT).setType("image/png")
            .putExtra(Intent.EXTRA_TITLE, name);
        launchPicker(intent, SAVE_PNG);
    }

    private void launchPicker(Intent intent, int request) {
        intent.addCategory(Intent.CATEGORY_OPENABLE);
        runOnUiThread(() -> {
            try {
                startActivityForResult(intent, request);
            } catch (RuntimeException error) {
                onDocument(request, -1, null, "Cannot open the Android file picker.");
            }
        });
    }

    @Override
    protected void onActivityResult(int request, int result, Intent data) {
        super.onActivityResult(request, result, data);
        if (request != OPEN_SER && request != SAVE_PNG) {
            return;
        }
        if (result != RESULT_OK || data == null || data.getData() == null) {
            onDocument(request, -1, null, null);
            return;
        }
        Uri uri = data.getData();
        new Thread(() -> openDocument(request, uri), "solex-document").start();
    }

    private void openDocument(int request, Uri uri) {
        try (ParcelFileDescriptor document =
                 getContentResolver().openFileDescriptor(uri, request == OPEN_SER ? "r" : "wt")) {
            if (document == null) {
                onDocument(request, -1, null, "The file provider did not open the document.");
                return;
            }
            onDocument(request, document.getFd(), displayName(uri, request), null);
        } catch (Exception error) {
            String detail = error.getLocalizedMessage();
            onDocument(request, -1, null, detail == null ? "Cannot access the document." : detail);
        }
    }

    private String displayName(Uri uri, int request) {
        try (Cursor cursor = getContentResolver().query(uri,
                 new String[] {OpenableColumns.DISPLAY_NAME}, null, null, null)) {
            if (cursor != null && cursor.moveToFirst() && !cursor.isNull(0)) {
                return cursor.getString(0);
            }
        } catch (RuntimeException ignored) {
            // Some providers can open a file without exposing its name.
        }
        return request == OPEN_SER ? "scan.ser" : "reconstruction.png";
    }
}
