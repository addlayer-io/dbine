package io.addlayer.dbine.mobile

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import java.io.File
import java.security.KeyStore
import java.security.SecureRandom
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

// The phone's pairing at rest (docs/mobile-security.md): the app's Rust
// seals pairing.json with a random file key, and that file key is sealed
// with an AES-256 key that lives in the Android Keystore and never leaves
// it. A copy of the app's files (a rooted phone, a backup) is useless
// without this phone's Keystore. If the Keystore key is gone (credentials
// cleared), a new one is made and the old pairing simply can't be opened:
// the user pairs again.
object KeyVault {
  private const val ALIAS = "dbine-file-key"
  private const val FILE = "file-key.sealed"

  init {
    System.loadLibrary("dbine_mobile_lib")
  }

  /** Hands the file key to the app's Rust (KeyVault_setFileKey in lib.rs). */
  @JvmStatic
  private external fun setFileKey(key: ByteArray)

  /** Before the Rust reads or writes the pairing: in the app and in NewsWorker. */
  @JvmStatic
  fun load(context: Context) {
    val f = File(context.dataDir, FILE)
    val key = try {
      if (f.exists()) open(f.readBytes()) else null
    } catch (_: Exception) {
      null
    } ?: ByteArray(32).also {
      SecureRandom().nextBytes(it)
      f.writeBytes(seal(it))
    }
    setFileKey(key)
    key.fill(0)
  }

  private fun secret(): SecretKey {
    val ks = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
    (ks.getKey(ALIAS, null) as? SecretKey)?.let { return it }
    val gen = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
    gen.init(
      KeyGenParameterSpec.Builder(ALIAS, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
        .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
        .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
        .setKeySize(256)
        .build()
    )
    return gen.generateKey()
  }

  private fun seal(plain: ByteArray): ByteArray {
    val c = Cipher.getInstance("AES/GCM/NoPadding")
    c.init(Cipher.ENCRYPT_MODE, secret())
    return c.iv + c.doFinal(plain)
  }

  private fun open(sealed: ByteArray): ByteArray {
    val c = Cipher.getInstance("AES/GCM/NoPadding")
    c.init(Cipher.DECRYPT_MODE, secret(), GCMParameterSpec(128, sealed, 0, 12))
    return c.doFinal(sealed, 12, sealed.size - 12)
  }
}
