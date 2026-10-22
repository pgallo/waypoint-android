# The native-call layer and generated bindings are reached reflectively.
-keep class com.sun.jna.** { *; }
-keep class * extends com.sun.jna.** { *; }
-keep class io.github.pgallo.waypoint.core.** { *; }
-dontwarn java.awt.**
