package org.mjsolex.mobile;

import android.app.NativeActivity;
import android.content.Intent;
import android.database.Cursor;
import android.net.Uri;
import android.os.Bundle;
import android.os.ParcelFileDescriptor;
import android.provider.DocumentsContract;
import android.provider.OpenableColumns;
import android.widget.Toast;
import java.io.FileNotFoundException;
import java.util.Locale;

public final class MainActivity extends NativeActivity {
    static {
        System.loadLibrary("mjsolex");
    }

    private static final int OPEN_SER = 10;
    private static final int SAVE_PNG = 11;
    private static final int SAVE_FITS = 12;
    private static final int OPEN_SHARED_SER = 13;
    private static final int OPEN_FLAT = 14;
    private static final int SAVE_RAW_PNG = 15;
    private static final int SAVE_BATCH = 16;
    private Uri pendingShared;

    private native boolean onDocument(
        int request, ParcelFileDescriptor document, String name, String error);
    private native void onExportDirectory(Uri directory);
    private native void onShareAvailable();

    public String getUiLanguage() {
        String preferred = getSharedPreferences("appearance", MODE_PRIVATE)
            .getString("language", null);
        if ("zh".equals(preferred) || "en".equals(preferred)) {
            return preferred;
        }
        Locale locale = getResources().getConfiguration().getLocales().get(0);
        return "zh".equals((locale == null ? Locale.getDefault() : locale).getLanguage()) ? "zh" : "en";
    }

    public void setUiLanguage(String language) {
        getSharedPreferences("appearance", MODE_PRIVATE).edit()
            .putString("language", language).apply();
    }

    private String uiText(String english, String chinese) {
        return "zh".equals(getUiLanguage()) ? chinese : english;
    }

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        getWindow().setStatusBarColor(0xff11151b);
        getWindow().setNavigationBarColor(0xff11151b);
        if (state == null) {
            acceptShare(getIntent());
        }
    }

    @Override
    protected void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        setIntent(intent);
        acceptShare(intent);
    }

    private void acceptShare(Intent intent) {
        if (intent == null || !Intent.ACTION_SEND.equals(intent.getAction())) {
            return;
        }
        try {
            Uri uri = intent.getParcelableExtra(Intent.EXTRA_STREAM);
            if (uri == null && intent.getClipData() != null && intent.getClipData().getItemCount() == 1) {
                uri = intent.getClipData().getItemAt(0).getUri();
            }
            if (uri != null && "content".equals(uri.getScheme())) {
                synchronized (this) {
                    pendingShared = uri;
                }
                onShareAvailable();
            }
        } catch (RuntimeException error) {
            Toast.makeText(this, uiText("The shared attachment could not be opened.",
                "无法打开分享的文件。"), Toast.LENGTH_SHORT).show();
        }
    }

    // Called from the render thread only when no picker or processing job is active.
    public boolean openSharedSer() {
        final Uri uri;
        synchronized (this) {
            uri = pendingShared;
            pendingShared = null;
        }
        if (uri == null) {
            return false;
        }
        new Thread(() -> openDocument(OPEN_SHARED_SER, uri), "solex-shared-document").start();
        return true;
    }

    public boolean openIndigo() {
        Intent intent = getPackageManager().getLaunchIntentForPackage("com.indigo.mobileobservatory");
        if (intent == null) {
            return false;
        }
        runOnUiThread(() -> {
            try {
                startActivity(intent);
            } catch (RuntimeException error) {
                Toast.makeText(this, uiText("Cannot open Indigo Observatory.",
                    "无法打开靛空台。"), Toast.LENGTH_SHORT).show();
            }
        });
        return true;
    }

    public void pickSer(int request) {
        Intent intent = new Intent(Intent.ACTION_OPEN_DOCUMENT).setType("*/*");
        launchPicker(intent, request);
    }

    public void createImage(int request, String name) {
        String type = request == SAVE_FITS ? "application/fits" : "image/png";
        Intent intent = new Intent(Intent.ACTION_CREATE_DOCUMENT).setType(type)
            .putExtra(Intent.EXTRA_TITLE, name);
        launchPicker(intent, request);
    }

    public void pickExportDirectory() {
        launchPicker(new Intent(Intent.ACTION_OPEN_DOCUMENT_TREE), SAVE_BATCH);
    }

    public Uri createExportFile(Uri directory, String mimeType, String name)
            throws FileNotFoundException {
        Uri parent = DocumentsContract.buildDocumentUriUsingTree(
            directory, DocumentsContract.getTreeDocumentId(directory));
        return DocumentsContract.createDocument(getContentResolver(), parent, mimeType, name);
    }

    public ParcelFileDescriptor openExportFile(Uri file) throws FileNotFoundException {
        return getContentResolver().openFileDescriptor(file, "wt");
    }

    public void deleteExportFile(Uri file) throws FileNotFoundException {
        DocumentsContract.deleteDocument(getContentResolver(), file);
    }

    private void launchPicker(Intent intent, int request) {
        if (request != SAVE_BATCH) {
            intent.addCategory(Intent.CATEGORY_OPENABLE);
        }
        runOnUiThread(() -> {
            try {
                startActivityForResult(intent, request);
            } catch (RuntimeException error) {
                onDocument(request, null, null, "picker");
            }
        });
    }

    @Override
    protected void onActivityResult(int request, int result, Intent data) {
        super.onActivityResult(request, result, data);
        if (request == SAVE_BATCH) {
            onExportDirectory(result == RESULT_OK && data != null ? data.getData() : null);
            return;
        }
        if (request != OPEN_SER && request != OPEN_FLAT
            && request != SAVE_PNG && request != SAVE_FITS && request != SAVE_RAW_PNG) {
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
            String name = displayName(uri, request);
            if (request == OPEN_SHARED_SER && !name.toLowerCase(Locale.ROOT).endsWith(".ser")) {
                onDocument(request, null, null, "single_ser");
                return;
            }
            document = getContentResolver().openFileDescriptor(uri, isInput(request) ? "r" : "wt");
            if (document == null) {
                onDocument(request, null, null, "unavailable");
                return;
            }
            if (onDocument(request, document, name, null)) {
                // Native code owns the descriptor and its provider status channel.
                document = null;
            }
        } catch (Exception error) {
            String detail = error.getLocalizedMessage();
            onDocument(request, null, null, detail == null ? "access" : "access:" + detail);
        } finally {
            if (document != null) {
                try {
                    if (isInput(request)) {
                        document.close();
                    } else {
                        document.closeWithError("MJSol'Ex did not accept this export");
                    }
                } catch (java.io.IOException ignored) {
                    // No native job owns this descriptor.
                }
            }
        }
    }

    private boolean isInput(int request) {
        return request == OPEN_SER || request == OPEN_SHARED_SER || request == OPEN_FLAT;
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
        if (request == OPEN_SHARED_SER && uri.getLastPathSegment() != null) {
            return uri.getLastPathSegment();
        }
        if (isInput(request)) {
            return request == OPEN_FLAT ? "flat.ser" : "scan.ser";
        }
        return request == SAVE_FITS ? "reconstruction.fits" : "reconstruction.png";
    }
}
