package io.berrykeep.android.ui

import android.database.MatrixCursor
import android.provider.DocumentsContract
import androidx.test.ext.junit.runners.AndroidJUnit4
import io.berrykeep.android.saf.BerryKeepDocumentColumns
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class CursorColumnsTest {
    @Test
    fun dottedCustomColumnName_resolvesByExactMatch() {
        val cursor = MatrixCursor(
            arrayOf(
                DocumentsContract.Document.COLUMN_DOCUMENT_ID,
                BerryKeepDocumentColumns.COLUMN_REMOTE_PATH,
                BerryKeepDocumentColumns.COLUMN_IMAGE_WIDTH,
            ),
        )
        cursor.addRow(arrayOf("file:gallery/cat.png", "gallery/cat.png", 640))

        cursor.moveToFirst()

        assertEquals("gallery/cat.png", cursor.stringOrNull(BerryKeepDocumentColumns.COLUMN_REMOTE_PATH))
        assertEquals(640, cursor.intOrNull(BerryKeepDocumentColumns.COLUMN_IMAGE_WIDTH))
    }

    @Test
    fun missingColumn_returnsNull() {
        val cursor = MatrixCursor(arrayOf(DocumentsContract.Document.COLUMN_DOCUMENT_ID))
        cursor.addRow(arrayOf("file:gallery/cat.png"))

        cursor.moveToFirst()

        assertNull(cursor.stringOrNull(BerryKeepDocumentColumns.COLUMN_THUMBNAIL_STATUS))
    }
}
