#import <Foundation/Foundation.h>
#include <stdint.h>
#include <sys/mount.h>
#include <sys/xattr.h>

struct eidos_file_metadata {
    uint64_t logical_size;
    uint64_t allocated_size;
    uint64_t volume_id_high;
    uint64_t volume_id_low;
    int ubiquitous;
    int placeholder;
    int resource_fork;
    int snapshot;
    int external_volume;
};

int eidos_query_file_metadata(const char *file_name,
                              struct eidos_file_metadata *output) {
    if (file_name == NULL || output == NULL) return 0;
    @autoreleasepool {
        NSURL *url = [NSURL fileURLWithFileSystemRepresentation:file_name
                                                    isDirectory:NO
                                                  relativeToURL:nil];
        NSError *error = nil;
        NSDictionary<NSURLResourceKey, id> *values = [url resourceValuesForKeys:@[
            NSURLFileSizeKey,
            NSURLFileAllocatedSizeKey,
            NSURLIsUbiquitousItemKey,
            NSURLUbiquitousItemDownloadingStatusKey,
            NSURLVolumeIsInternalKey
        ] error:&error];
        if (values == nil || error != nil) return 0;
        NSNumber *logical = values[NSURLFileSizeKey];
        NSNumber *allocated = values[NSURLFileAllocatedSizeKey];
        NSNumber *ubiquitous = values[NSURLIsUbiquitousItemKey];
        NSString *status = values[NSURLUbiquitousItemDownloadingStatusKey];
        NSNumber *internal = values[NSURLVolumeIsInternalKey];
        output->logical_size = logical == nil ? 0 : logical.unsignedLongLongValue;
        output->allocated_size = allocated == nil ? 0 : allocated.unsignedLongLongValue;
        output->ubiquitous = ubiquitous.boolValue ? 1 : 0;
        output->placeholder = output->ubiquitous &&
            status != nil &&
            ![status isEqualToString:NSURLUbiquitousItemDownloadingStatusCurrent];
        output->resource_fork = !output->placeholder &&
            getxattr(file_name, "com.apple.ResourceFork",
                NULL, 0, 0, XATTR_NOFOLLOW) >= 0;
        struct statfs fs;
        if (statfs(file_name, &fs) == 0) {
            output->volume_id_high = (uint32_t)fs.f_fsid.val[0];
            output->volume_id_low = (uint32_t)fs.f_fsid.val[1];
            output->snapshot = (fs.f_flags & MNT_SNAPSHOT) != 0;
            output->external_volume = internal == nil
                ? (fs.f_flags & MNT_REMOVABLE) != 0
                : !internal.boolValue;
        }
        return 1;
    }
}
