# R8 rules for the release build.
#
# The native engine finds these by name: the `external` functions through
# their JNI symbol names, and the listener's `onEvent` through a method
# lookup. Renaming or removing either breaks the app at run time only.
-keep class dev.ccptr.soundbridge.Native { *; }
-keep interface dev.ccptr.soundbridge.Native$Listener { *; }
-keepclassmembers class * implements dev.ccptr.soundbridge.Native$Listener {
    void onEvent(java.lang.String);
}
