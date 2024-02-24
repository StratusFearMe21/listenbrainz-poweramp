package com.example.listenbrainzpoweramp

import android.app.Activity
import android.app.AlertDialog
import android.app.NotificationManager
import android.content.Context
import android.content.Intent
import android.content.SharedPreferences
import android.content.res.Configuration
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.PowerManager
import android.provider.DocumentsContract
import android.provider.Settings
import android.util.Log
import androidx.activity.result.ActivityResultLauncher
import androidx.activity.result.contract.ActivityResultContract
import androidx.activity.result.contract.ActivityResultContracts
import androidx.annotation.CallSuper
import androidx.appcompat.app.AppCompatActivity
import androidx.core.content.ContextCompat
import androidx.preference.Preference
import androidx.preference.PreferenceFragmentCompat
import androidx.preference.PreferenceManager
import java.util.jar.Manifest

class SettingsActivity : AppCompatActivity() {
    private lateinit var requestPermissionLauncher: ActivityResultLauncher<String>
    private lateinit var documentTreeOpener: ActivityResultLauncher<Uri?>

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.settings_activity)
        if (savedInstanceState == null) {
            supportFragmentManager
                .beginTransaction()
                .replace(R.id.settings, SettingsFragment(intent))
                .commit()
        }
        supportActionBar?.setDisplayHomeAsUpEnabled(true)
        val serviceIntent = Intent(applicationContext, ForegroundService::class.java)
        requestPermissionLauncher = registerForActivityResult(ActivityResultContracts.RequestPermission()) {
        }
        if (!(getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager).areNotificationsEnabled()) {
            requestPermissionLauncher.launch(android.Manifest.permission.POST_NOTIFICATIONS)
        }
        val intent = Intent()
        val packageName = packageName
        val pm = getSystemService(POWER_SERVICE) as PowerManager
        if (!pm.isIgnoringBatteryOptimizations(packageName)) {
            intent.action = Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS
            intent.data = Uri.parse("package:$packageName")
            startActivity(intent)
        }
        if (applicationContext.contentResolver.persistedUriPermissions.isEmpty()) {
            documentTreeOpener = registerForActivityResult(ActivityResultContracts.OpenDocumentTree()) {
                it?.let { root ->
                    applicationContext.contentResolver.takePersistableUriPermission(
                        root, Intent.FLAG_GRANT_READ_URI_PERMISSION)

                    Log.v("SettingsFragment", "${root.toString()}")

                }
            }
            // Create the object of AlertDialog Builder class
            val builder = AlertDialog.Builder(this)

            // Set the message show for the Alert time
            builder.setMessage("ListenBrainz PowerAmp reads your music files directly in order to get essential metadata, such as MBIDs. It needs permissions to read inside your music library to do this")

            // Set Alert Title
            builder.setTitle("We need permissions")

            // Set Cancelable false for when the user clicks on the outside the Dialog Box then it will remain show
            builder.setCancelable(false)

            // Set the positive button with yes name Lambda OnClickListener method is use of DialogInterface interface.
            builder.setPositiveButton("Add a music directory") {
                // When the user click yes button then app will close
                _, _ -> documentTreeOpener.launch(null)
            }

            // Create the Alert dialog
            val alertDialog = builder.create()
            // Show the Alert Dialog box
            alertDialog.show()
        }
        applicationContext.startForegroundService(serviceIntent)
    }

    class SettingsFragment(intent: Intent) : PreferenceFragmentCompat() {
        private lateinit var documentTreeOpener: ActivityResultLauncher<Uri?>
        val error: String? = intent.getStringExtra("error")

        override fun onCreatePreferences(savedInstanceState: Bundle?, rootKey: String?) {
            if (error != null) {
                val sharedPreferences = PreferenceManager.getDefaultSharedPreferences(this.requireContext())
                sharedPreferences.edit().putString("error", error).commit()
            }
            setPreferencesFromResource(R.xml.root_preferences, rootKey)
            documentTreeOpener = registerForActivityResult(ActivityResultContracts.OpenDocumentTree()) {
                it?.let { root ->
                    requireContext().contentResolver.takePersistableUriPermission(
                        root, Intent.FLAG_GRANT_READ_URI_PERMISSION)

                    Log.v("SettingsFragment", "${root.toString()}")

                }
            }
            val button: Preference = findPreference("dirperm")!!
            button.onPreferenceClickListener =
                Preference.OnPreferenceClickListener { //code for what you want it to do
                    documentTreeOpener.launch(null)
                    true
                }
        }
    }
}