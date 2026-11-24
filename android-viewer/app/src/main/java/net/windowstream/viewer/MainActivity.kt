package net.windowstream.viewer

import android.app.Activity
import android.content.Intent
import android.os.Bundle
import android.widget.Button
import android.widget.EditText
import org.json.JSONObject
import java.io.File

/**
 * Connection form for the viewer.
 *
 * On Connect it writes `viewer_config.json` (url + cert_hash) into the
 * app's internal files directory — the Rust side reads it from its
 * internal data path at `android_main` — and starts [ViewerActivity].
 * The Demo button removes the config, which puts the Rust side into
 * its in-process pattern mode (no host needed).
 *
 * The last used values are kept in SharedPreferences so the form is
 * prefilled on the next launch.
 */
class MainActivity : Activity() {

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)

        val prefs = getSharedPreferences(PREFS, MODE_PRIVATE)
        val urlField = findViewById<EditText>(R.id.url_field)
        val certField = findViewById<EditText>(R.id.cert_field)
        urlField.setText(prefs.getString(KEY_URL, ""))
        certField.setText(prefs.getString(KEY_CERT, ""))

        findViewById<Button>(R.id.connect_button).setOnClickListener {
            val url = urlField.text.toString().trim()
            val cert = certField.text.toString().trim()
            prefs.edit().putString(KEY_URL, url).putString(KEY_CERT, cert).apply()
            val config = JSONObject()
                .put("url", url)
                .put("cert_hash", cert)
            File(filesDir, CONFIG_FILE).writeText(config.toString())
            startActivity(Intent(this, ViewerActivity::class.java))
        }

        findViewById<Button>(R.id.demo_button).setOnClickListener {
            File(filesDir, CONFIG_FILE).delete()
            startActivity(Intent(this, ViewerActivity::class.java))
        }
    }

    companion object {
        private const val PREFS = "viewer"
        private const val KEY_URL = "url"
        private const val KEY_CERT = "cert_hash"
        private const val CONFIG_FILE = "viewer_config.json"
    }
}
