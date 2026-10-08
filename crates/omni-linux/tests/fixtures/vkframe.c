/* A game-like frame loop over the guest's Vulkan driver, for the forwarding's A/Bs (vk_fast,
 * vk_batch): THREADS threads, each one frame after another --
 *   reset its descriptor pool, allocate 32 descriptor sets, update them (one call);
 *   create a buffer and destroy it again (a transient resource);
 *   allocate a command buffer, record DRAWS "draws" of 6 commands each (bind descriptor sets,
 *   push constants, bind vertex and index buffers, set viewport and scissor: ~3,000 commands),
 *   end it, submit it with a fence (the queue shared, under a lock), wait, reset the fence,
 *   free the command buffer.
 * No pipeline is bound (nothing is drawn): the commands only record, as most of a frame's do.
 *
 * Prints "vkframe ok <us per frame> <commands per frame> <frames>" and exits 0, or names the
 * failed step and exits 1. */
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
#include <vulkan/vulkan.h>

#define CHECK(step, call)                                         \
    do {                                                          \
        VkResult r_ = (call);                                     \
        if (r_ != VK_SUCCESS) {                                   \
            printf("%s failed %d\n", step, r_);                   \
            return 1;                                             \
        }                                                         \
    } while (0)

enum { THREADS = 3, FRAMES = 150, DRAWS = 500, SETS = 32 };

static VkPhysicalDevice g_pd;
static VkDevice g_device;
static VkQueue g_queue;
static uint32_t g_family;
static VkBuffer g_buffer;
static VkDescriptorSetLayout g_set_layout;
static VkPipelineLayout g_layout;
static pthread_mutex_t g_queue_lock = PTHREAD_MUTEX_INITIALIZER;
static double g_us[THREADS];
static int g_failed;

static uint32_t memory_type(uint32_t bits, VkMemoryPropertyFlags want) {
    VkPhysicalDeviceMemoryProperties mp;
    vkGetPhysicalDeviceMemoryProperties(g_pd, &mp);
    for (uint32_t i = 0; i < mp.memoryTypeCount; i++) {
        if ((bits & (1u << i)) && (mp.memoryTypes[i].propertyFlags & want) == want) return i;
    }
    return UINT32_MAX;
}

static double now_us(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (double)t.tv_sec * 1e6 + (double)t.tv_nsec / 1e3;
}

static int frames(int me) {
    VkDevice d = g_device;
    VkCommandPoolCreateInfo pci = {.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, .queueFamilyIndex = g_family};
    VkCommandPool pool;
    CHECK("vkCreateCommandPool", vkCreateCommandPool(d, &pci, NULL, &pool));
    VkDescriptorPoolSize size = {VK_DESCRIPTOR_TYPE_UNIFORM_BUFFER, SETS};
    VkDescriptorPoolCreateInfo dpci = {.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO, .maxSets = SETS, .poolSizeCount = 1,
                                       .pPoolSizes = &size};
    VkDescriptorPool dpool;
    CHECK("vkCreateDescriptorPool", vkCreateDescriptorPool(d, &dpci, NULL, &dpool));
    VkFenceCreateInfo fci = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
    VkFence fence;
    CHECK("vkCreateFence", vkCreateFence(d, &fci, NULL, &fence));
    VkDescriptorSetLayout layouts[SETS];
    for (int i = 0; i < SETS; i++) layouts[i] = g_set_layout;
    double t0 = now_us();
    for (int f = 0; f < FRAMES; f++) {
        CHECK("vkResetDescriptorPool", vkResetDescriptorPool(d, dpool, 0));
        VkDescriptorSetAllocateInfo dsai = {.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO, .descriptorPool = dpool,
                                            .descriptorSetCount = SETS, .pSetLayouts = layouts};
        VkDescriptorSet sets[SETS];
        CHECK("vkAllocateDescriptorSets", vkAllocateDescriptorSets(d, &dsai, sets));
        VkDescriptorBufferInfo infos[SETS];
        VkWriteDescriptorSet writes[SETS];
        for (int i = 0; i < SETS; i++) {
            infos[i] = (VkDescriptorBufferInfo){g_buffer, (VkDeviceSize)(i * 256), 256};
            writes[i] = (VkWriteDescriptorSet){.sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET, .dstSet = sets[i], .descriptorCount = 1,
                                               .descriptorType = VK_DESCRIPTOR_TYPE_UNIFORM_BUFFER, .pBufferInfo = &infos[i]};
        }
        vkUpdateDescriptorSets(d, SETS, writes, 0, NULL);

        VkBufferCreateInfo bci = {.sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .size = 4096, .usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT};
        VkBuffer transient;
        CHECK("vkCreateBuffer(transient)", vkCreateBuffer(d, &bci, NULL, &transient));
        vkDestroyBuffer(d, transient, NULL);

        VkCommandBufferAllocateInfo cai = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, .commandPool = pool,
                                           .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1};
        VkCommandBuffer cb;
        CHECK("vkAllocateCommandBuffers", vkAllocateCommandBuffers(d, &cai, &cb));
        VkCommandBufferBeginInfo begin = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO, .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT};
        CHECK("vkBeginCommandBuffer", vkBeginCommandBuffer(cb, &begin));
        for (int i = 0; i < DRAWS; i++) {
            float constants[16];
            for (int k = 0; k < 16; k++) constants[k] = (float)(i + k);
            VkDeviceSize offset = (VkDeviceSize)(i & 15) * 64;
            VkViewport vp = {0, 0, 64, 64, 0, 1};
            VkRect2D sc = {{0, 0}, {64, 64}};
            vkCmdBindDescriptorSets(cb, VK_PIPELINE_BIND_POINT_GRAPHICS, g_layout, 0, 1, &sets[i % SETS], 0, NULL);
            vkCmdPushConstants(cb, g_layout, VK_SHADER_STAGE_VERTEX_BIT, 0, sizeof constants, constants);
            vkCmdBindVertexBuffers(cb, 0, 1, &g_buffer, &offset);
            vkCmdBindIndexBuffer(cb, g_buffer, offset, VK_INDEX_TYPE_UINT16);
            vkCmdSetViewport(cb, 0, 1, &vp);
            vkCmdSetScissor(cb, 0, 1, &sc);
        }
        CHECK("vkEndCommandBuffer", vkEndCommandBuffer(cb));
        VkSubmitInfo si = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &cb};
        pthread_mutex_lock(&g_queue_lock);
        VkResult sr = vkQueueSubmit(g_queue, 1, &si, fence);
        pthread_mutex_unlock(&g_queue_lock);
        CHECK("vkQueueSubmit", sr);
        CHECK("vkWaitForFences", vkWaitForFences(d, 1, &fence, VK_TRUE, 10ull * 1000 * 1000 * 1000));
        CHECK("vkResetFences", vkResetFences(d, 1, &fence));
        vkFreeCommandBuffers(d, pool, 1, &cb);
    }
    g_us[me] = (now_us() - t0) / FRAMES;
    vkDestroyFence(d, fence, NULL);
    vkDestroyDescriptorPool(d, dpool, NULL);
    vkDestroyCommandPool(d, pool, NULL);
    return 0;
}

static void* thread_main(void* arg) {
    int me = (int)(intptr_t)arg;
    if (frames(me) != 0) g_failed = 1;
    return NULL;
}

int main(void) {
    VkApplicationInfo app = {.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO, .pApplicationName = "vkframe", .apiVersion = VK_API_VERSION_1_1};
    VkInstanceCreateInfo ici = {.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app};
    VkInstance instance;
    CHECK("vkCreateInstance", vkCreateInstance(&ici, NULL, &instance));
    uint32_t n = 1;
    VkResult er = vkEnumeratePhysicalDevices(instance, &n, &g_pd);
    if ((er != VK_SUCCESS && er != VK_INCOMPLETE) || n == 0) {
        printf("vkEnumeratePhysicalDevices failed %d\n", er);
        return 1;
    }
    uint32_t nq = 16;
    VkQueueFamilyProperties qf[16];
    vkGetPhysicalDeviceQueueFamilyProperties(g_pd, &nq, qf);
    g_family = UINT32_MAX;
    for (uint32_t i = 0; i < nq && g_family == UINT32_MAX; i++) {
        if (qf[i].queueFlags & VK_QUEUE_GRAPHICS_BIT) g_family = i;
    }
    float priority = 1.0f;
    VkDeviceQueueCreateInfo qci = {.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, .queueFamilyIndex = g_family, .queueCount = 1,
                                   .pQueuePriorities = &priority};
    VkDeviceCreateInfo dci = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci};
    CHECK("vkCreateDevice", vkCreateDevice(g_pd, &dci, NULL, &g_device));
    vkGetDeviceQueue(g_device, g_family, 0, &g_queue);

    VkBufferCreateInfo bci = {.sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .size = 1 << 16,
                              .usage = VK_BUFFER_USAGE_UNIFORM_BUFFER_BIT | VK_BUFFER_USAGE_VERTEX_BUFFER_BIT | VK_BUFFER_USAGE_INDEX_BUFFER_BIT};
    CHECK("vkCreateBuffer", vkCreateBuffer(g_device, &bci, NULL, &g_buffer));
    VkMemoryRequirements req;
    vkGetBufferMemoryRequirements(g_device, g_buffer, &req);
    VkMemoryAllocateInfo mai = {.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = req.size,
                                .memoryTypeIndex = memory_type(req.memoryTypeBits, 0)};
    VkDeviceMemory mem;
    CHECK("vkAllocateMemory", vkAllocateMemory(g_device, &mai, NULL, &mem));
    CHECK("vkBindBufferMemory", vkBindBufferMemory(g_device, g_buffer, mem, 0));
    VkDescriptorSetLayoutBinding binding = {0, VK_DESCRIPTOR_TYPE_UNIFORM_BUFFER, 1, VK_SHADER_STAGE_VERTEX_BIT, NULL};
    VkDescriptorSetLayoutCreateInfo dslci = {.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO, .bindingCount = 1, .pBindings = &binding};
    CHECK("vkCreateDescriptorSetLayout", vkCreateDescriptorSetLayout(g_device, &dslci, NULL, &g_set_layout));
    VkPushConstantRange range = {VK_SHADER_STAGE_VERTEX_BIT, 0, 64};
    VkPipelineLayoutCreateInfo plci = {.sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO, .setLayoutCount = 1, .pSetLayouts = &g_set_layout,
                                       .pushConstantRangeCount = 1, .pPushConstantRanges = &range};
    CHECK("vkCreatePipelineLayout", vkCreatePipelineLayout(g_device, &plci, NULL, &g_layout));

    pthread_t threads[THREADS];
    for (int i = 0; i < THREADS; i++) pthread_create(&threads[i], NULL, thread_main, (void*)(intptr_t)i);
    for (int i = 0; i < THREADS; i++) pthread_join(threads[i], NULL);
    if (g_failed) return 1;
    double us = 0;
    for (int i = 0; i < THREADS; i++) us += g_us[i];
    printf("vkframe ok %.1f %d %d\n", us / THREADS, DRAWS * 6 + 9, FRAMES);

    vkDestroyPipelineLayout(g_device, g_layout, NULL);
    vkDestroyDescriptorSetLayout(g_device, g_set_layout, NULL);
    vkDestroyBuffer(g_device, g_buffer, NULL);
    vkFreeMemory(g_device, mem, NULL);
    vkDestroyDevice(g_device, NULL);
    vkDestroyInstance(instance, NULL);
    fflush(stdout);
    return 0;
}
