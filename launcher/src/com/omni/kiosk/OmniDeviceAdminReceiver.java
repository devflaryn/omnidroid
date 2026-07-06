package com.omni.kiosk;

import android.app.admin.DeviceAdminReceiver;

/** Device-admin receiver so the kiosk can be set as device owner
 *  (dpm set-device-owner) and drive Lock Task Mode. No policy callbacks
 *  are needed — its presence + the device_admin.xml policy is enough. */
public class OmniDeviceAdminReceiver extends DeviceAdminReceiver {
}
