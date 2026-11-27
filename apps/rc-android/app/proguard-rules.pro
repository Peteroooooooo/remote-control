-keep class com.rc.android.nativebridge.** { *; }
-keep interface com.rc.android.nativebridge.NativeCallback { *; }
-keepclassmembers class * implements com.rc.android.nativebridge.NativeCallback {
    <methods>;
}
