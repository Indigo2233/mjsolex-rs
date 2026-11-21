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
    private static final int SAVE_FITS = 12;

    private native boolean onDocument(
        int request, ParcelFileDescriptor document, String name, String error);

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

    public void createImage(int request, String name) {
        String type = request == SAVE_FITS ? "application/fits" : "image/png";
        Intent intent = new Intent(Intent.ACTION_CREATE_DOCUMENT).setType(type)
            .putExtra(Intent.EXTRA_TITLE, name);
        launchPicker(intent, request);
    }

    private void launchPicker(Intent intent, int request) {
        intent.addCategory(Intent.CATEGORY_OPENABLE);
        runOnUiThread(() -> {
            try {
                startActivityForResult(intent, request);
            } catch (RuntimeException error) {
                onDocument(request, null, null, "Cannot open the Android file picker.");
            }
        });
    }

    @Override
    protected void onActivityResult(int request, int result, Intent data) {
        super.onActivityResult(request, result, data);
        if (request != OPEN_SER && request != SAVE_PNG && request != SAVE_FITS) {
            return;
        }
        if (result != RESULT_OK || data == null || data.getData() == null) {
            onDocument(request, null, null, null);
            return;
        }
        Uri uri = data.getData();
        new Thread(() -> openDocument(request, uri), "solex-document").start();
    }

    private void openDocument(int request, Uri uri) {
        ParcelFileDescriptor document = null;
        try {
            document = getContentResolver().openFileDescriptor(uri, request == OPEN_SER ? "r" : "wt");
            if (document == null) {
                onDocument(request, null, null, "The file provider did not open the document.");
                return;
            }
            if (onDocument(request, document, displayName(uri, request), null)) {
                // Native code owns the descriptor and its provider status channel.
                document = null;
            }
        } catch (Exception error) {
            String detail = error.getLocalizedMessage();
            onDocument(request, null, null, detail == null ? "Cannot access the document." : detail);
        } finally {
            if (document != null) {
                try {
                    if (request == OPEN_SER) {
                        document.close();
                    } else {
                        document.closeWithError("MJ Sol'Ex did not accept this export");
                    }
                } catch (java.io.IOException ignored) {
                    // No native job owns this descriptor.
                }
            }
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
        if (request == OPEN_SER) {
            return "scan.ser";
        }
        return request == SAVE_FITS ? "reconstruction.fits" : "reconstruction.png";
    }
}
