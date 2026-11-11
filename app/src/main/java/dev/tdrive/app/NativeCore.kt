package dev.tdrive.app

object NativeCore {
    init {
        System.loadLibrary("tdrive_core")
    }

    external fun nativeInit(dataDir: String): String
    external fun nativeSpawn(): String
    external fun nativeDispatch(json: String): String
    external fun nativePoll(): String?
}
