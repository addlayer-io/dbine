# Add project specific ProGuard rules here.
# You can control the set of applied configuration files using the
# proguardFiles setting in build.gradle.
#
# For more details, see
#   http://developer.android.com/guide/developing/tools/proguard.html

# If your project uses WebView with JS, uncomment the following
# and specify the fully qualified class name to the JavaScript interface
# class:
#-keepclassmembers class fqcn.of.javascript.interface.for.webview {
#   public *;
#}

# Uncomment this to preserve the line number information for
# debugging stack traces.
#-keepattributes SourceFile,LineNumberTable

# If you keep the line number information, uncomment this to
# hide the original source file name.
#-renamesourcefileattribute SourceFile
# NewsWorker.kt: its native `check` is bound by name to the Rust library
# (Java_io_addlayer_dbine_mobile_NewsWorker_check), and WorkManager creates
# it by class name.
-keep class io.addlayer.dbine.mobile.NewsWorker { *; }

# KeyVault.kt: its native setFileKey is bound by name to the Rust library.
-keep class io.addlayer.dbine.mobile.KeyVault { *; }
