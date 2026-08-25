#import <Foundation/Foundation.h>
#include <stdint.h>

struct eidos_file_metadata {
    uint64_t logical_size;
    uint64_t allocated_size;
    int ubiquitous;
    int placeholder;
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
            NSURLUbiquitousItemDownloadingStatusKey
        ] error:&error];
        if (values == nil || error != nil) return 0;
        NSNumber *logical = values[NSURLFileSizeKey];
        NSNumber *allocated = values[NSURLFileAllocatedSizeKey];
        NSNumber *ubiquitous = values[NSURLIsUbiquitousItemKey];
        NSString *status = values[NSURLUbiquitousItemDownloadingStatusKey];
        output->logical_size = logical == nil ? 0 : logical.unsignedLongLongValue;
        output->allocated_size = allocated == nil ? 0 : allocated.unsignedLongLongValue;
        output->ubiquitous = ubiquitous.boolValue ? 1 : 0;
        output->placeholder = output->ubiquitous &&
            status != nil &&
            ![status isEqualToString:NSURLUbiquitousItemDownloadingStatusCurrent];
        return 1;
    }
}
