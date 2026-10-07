import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { configureI18n } from "../../i18n";
import { createAppUpdateController } from "../../lib/appUpdateController";
import type { AppUpdateBackend } from "../../lib/appUpdateController";
import type {
  AisdkComponentStatus,
  AppComponentsStatus,
  AppReleaseAsset,
  AppUpdateCheck,
  AppUpdateDownload,
  AppUpdateDownloadEvent,
  AppVersionInfo
} from "../../types";

const updateMocks = vi.hoisted(() => ({
  backendConnected: true,
  appVersionInfo: vi.fn<() => Promise<AppVersionInfo>>(),
  appComponentsStatus: vi.fn<() => Promise<AppComponentsStatus>>(),
  checkAppUpdate: vi.fn<() => Promise<AppUpdateCheck>>(),
  downloadAppUpdate: vi.fn(),
  cancelAppUpdateDownload: vi.fn<() => Promise<void>>(),
  installAppUpdate: vi.fn()
}));

vi.mock("../../lib/backend", () => ({
  hasBackendRuntime: () => updateMocks.backendConnected
}));
vi.mock("../../lib/runtime", () => updateMocks);

import { UpdateSettings } from ".";

const versionInfo: AppVersionInfo = {
  version: "1.0.0",
  flavor: "installer",
  developmentBuild: false,
  arch: "x86_64",
  os: "windows",
  repositoryUrl: "https://github.com/catblob-hash/Mewrk",
  releasesUrl: "https://github.com/catblob-hash/Mewrk/releases",
  executableDir: "C:\\Program Files\\Mewrk"
};

const installedAisdk: AisdkComponentStatus = {
  state: "installed",
  protocol: 17,
  version: "1.2.4",
  builtAt: "2026-10-07T08:00:00Z",
  receivedBytes: null,
  totalBytes: null,
  error: null
};

const updateAsset: AppReleaseAsset = {
  name: "Mewrk_1.1.0_x64-setup.exe",
  downloadUrl: "https://github.com/catblob-hash/Mewrk/releases/download/v1.1.0/Mewrk_1.1.0_x64-setup.exe",
  size: 36_175_872
};

const upToDateCheck: AppUpdateCheck = {
  currentVersion: "1.0.0",
  latestVersion: "1.0.0",
  updateAvailable: false,
  release: {
    tag: "v1.0.0",
    name: "Mewrk v1.0.0",
    htmlUrl: "https://github.com/catblob-hash/Mewrk/releases/tag/v1.0.0",
    notes: "",
    publishedAt: "2026-09-01T00:00:00Z"
  },
  asset: null,
  checksumsAsset: null,
  checkedAt: "2026-09-05T12:00:00Z",
  inAppInstall: true
};

const availableCheck: AppUpdateCheck = {
  currentVersion: "1.0.0",
  latestVersion: "1.1.0",
  updateAvailable: true,
  release: {
    tag: "v1.1.0",
    name: "Mewrk v1.1.0",
    htmlUrl: "https://github.com/catblob-hash/Mewrk/releases/tag/v1.1.0",
    notes: "## What's new\n\n- faster",
    publishedAt: "2026-09-04T00:00:00Z"
  },
  asset: updateAsset,
  checksumsAsset: {
    name: "SHA256SUMS",
    downloadUrl: "https://github.com/catblob-hash/Mewrk/releases/download/v1.1.0/SHA256SUMS",
    size: 128
  },
  checkedAt: "2026-09-05T12:00:00Z",
  inAppInstall: true
};

const downloadedUpdate: AppUpdateDownload = {
  path: "C:\\Users\\me\\Downloads\\Mewrk_1.1.0_x64-setup.exe",
  fileName: updateAsset.name,
  sizeBytes: updateAsset.size,
  sha256: "a".repeat(64),
  verification: "verified",
  flavor: "installer"
};

function createBackend() {
  return {
    checkAppUpdate: vi.fn<() => Promise<AppUpdateCheck>>().mockResolvedValue(upToDateCheck),
    downloadAppUpdate: vi.fn<(
      asset: AppReleaseAsset,
      checksumsAsset: AppReleaseAsset | null,
      onProgress: (event: AppUpdateDownloadEvent) => void
    ) => Promise<AppUpdateDownload>>().mockResolvedValue(downloadedUpdate),
    cancelAppUpdateDownload: vi.fn<() => Promise<void>>().mockResolvedValue(undefined),
    installAppUpdate: vi.fn<(path: string) => Promise<{ action: "installer_launched" | "revealed" }>>()
      .mockResolvedValue({ action: "installer_launched" })
  } satisfies AppUpdateBackend;
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((nextResolve, nextReject) => {
    resolve = nextResolve;
    reject = nextReject;
  });
  return { promise, resolve, reject };
}

afterEach(() => configureI18n("zh-CN"));

describe("UpdateSettings", () => {
  beforeEach(() => {
    configureI18n("zh-CN");
    updateMocks.backendConnected = true;
    updateMocks.appVersionInfo.mockReset().mockResolvedValue(versionInfo);
    updateMocks.appComponentsStatus.mockReset().mockResolvedValue({ aisdk: installedAisdk });
    updateMocks.checkAppUpdate.mockReset();
    updateMocks.downloadAppUpdate.mockReset();
    updateMocks.cancelAppUpdateDownload.mockReset().mockResolvedValue(undefined);
    updateMocks.installAppUpdate.mockReset();
  });

  it("shows an offline version card without attempting a backend call", () => {
    updateMocks.backendConnected = false;
    const backend = createBackend();
    const { container } = render(
      <UpdateSettings controller={createAppUpdateController(backend)} />
    );

    expect(screen.getByTestId("current-version")).toHaveTextContent("—");
    expect(screen.getByText("当前预览没有连接应用后端，无法读取版本信息或检查更新。")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "检查更新" })).not.toBeInTheDocument();
    expect(container.querySelector(".update-settings__update")).not.toBeInTheDocument();
    expect(updateMocks.appVersionInfo).not.toHaveBeenCalled();
    expect(updateMocks.appComponentsStatus).not.toHaveBeenCalled();
    expect(screen.queryByText("AI SDK 组件")).not.toBeInTheDocument();
    expect(backend.checkAppUpdate).not.toHaveBeenCalled();
  });

  it("loads connected version details and reports an up-to-date release", async () => {
    const backend = createBackend();
    render(<UpdateSettings controller={createAppUpdateController(backend)} />);

    await screen.findByText("已是最新版本");
    await waitFor(() => {
      expect(screen.getByTestId("current-version")).toHaveTextContent("v1.0.0");
    });
    expect(screen.getByText("安装版")).toBeInTheDocument();
    expect(screen.getByText("x86_64")).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "源代码" })).toHaveAttribute("href", versionInfo.repositoryUrl);
    expect(screen.getByRole("link", { name: "源代码" })).toHaveAttribute("target", "_blank");
    expect(screen.getByRole("link", { name: "全部发布" })).toHaveAttribute("href", versionInfo.releasesUrl);
    expect(screen.getByRole("link", { name: "全部发布" })).toHaveAttribute("target", "_blank");
    expect(backend.checkAppUpdate).toHaveBeenCalledTimes(1);
  });

  it("leaves an MSIX install from the Store to the Microsoft Store and never checks GitHub", async () => {
    updateMocks.appVersionInfo.mockResolvedValue({
      ...versionInfo,
      flavor: "store_msix",
      executableDir: "C:\\Program Files\\WindowsApps\\Mewrk_1.0.0.0_x64__abc"
    });
    const backend = createBackend();
    render(<UpdateSettings controller={createAppUpdateController(backend)} />);

    expect(await screen.findByText("由 Microsoft Store 更新")).toBeInTheDocument();
    expect(screen.getByText("MSIX（Microsoft Store）")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "检查更新" })).not.toBeInTheDocument();
    expect(screen.queryByText("正在检查更新…")).not.toBeInTheDocument();
    expect(screen.getByText("这个版本的更新由 Microsoft Store 提供。")).toBeInTheDocument();
    expect(screen.queryByText(/GitHub Releases/)).not.toBeInTheDocument();
    expect(screen.queryByRole("link", { name: "全部发布" })).not.toBeInTheDocument();
    expect(backend.checkAppUpdate).not.toHaveBeenCalled();
  });

  it("renders an available release, its asset, and markdown notes", async () => {
    const backend = createBackend();
    backend.checkAppUpdate.mockResolvedValue(availableCheck);
    render(<UpdateSettings controller={createAppUpdateController(backend)} />);

    expect(await screen.findByText("发现新版本 v1.1.0")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "下载更新（34.5 MiB）" })).toBeInTheDocument();
    const notes = screen.getByLabelText("发布说明");
    expect(within(notes).getByRole("heading", { name: "What's new" })).toBeInTheDocument();
    expect(within(notes).getByRole("link", { name: "在 GitHub 上查看" })).toHaveAttribute(
      "href",
      availableCheck.release.htmlUrl
    );
  });

  it("shows download progress, verification, cancellation, and installer handoff", async () => {
    const user = userEvent.setup();
    const backend = createBackend();
    const download = deferred<AppUpdateDownload>();
    let onProgress: ((event: AppUpdateDownloadEvent) => void) | undefined;
    backend.checkAppUpdate.mockResolvedValue(availableCheck);
    backend.downloadAppUpdate.mockImplementation((_asset, _checksumsAsset, progress) => {
      onProgress = progress;
      return download.promise;
    });
    render(<UpdateSettings controller={createAppUpdateController(backend)} />);

    await user.click(await screen.findByRole("button", { name: "下载更新（34.5 MiB）" }));
    await waitFor(() => expect(backend.downloadAppUpdate).toHaveBeenCalledWith(
      updateAsset,
      availableCheck.checksumsAsset,
      expect.any(Function)
    ));

    act(() => onProgress?.({
      type: "progress",
      receivedBytes: 18_087_936,
      totalBytes: 36_175_872
    }));
    expect(screen.getByRole("progressbar")).toHaveAttribute("aria-valuenow", "50");
    expect(screen.getByText(/50%/)).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "取消下载" }));
    await waitFor(() => expect(backend.cancelAppUpdateDownload).toHaveBeenCalledTimes(1));

    act(() => onProgress?.({ type: "verifying" }));
    expect(screen.getByText("正在校验…")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "取消下载" })).toBeDisabled();

    await act(async () => {
      download.resolve(downloadedUpdate);
      await download.promise;
    });
    expect(await screen.findByText(/SHA-256 与发布的校验和一致/)).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "安装并重启" }));
    await waitFor(() => expect(backend.installAppUpdate).toHaveBeenCalledWith(downloadedUpdate.path));
  });

  it("keeps the portable reveal action after revealing its downloaded archive", async () => {
    const user = userEvent.setup();
    const backend = createBackend();
    const portableInfo = { ...versionInfo, flavor: "portable" as const };
    const portableDownload = {
      ...downloadedUpdate,
      path: "C:\\Users\\me\\Downloads\\Mewrk_1.1.0_windows_x64.zip",
      fileName: "Mewrk_1.1.0_windows_x64.zip",
      flavor: "portable" as const
    };
    updateMocks.appVersionInfo.mockResolvedValue(portableInfo);
    backend.checkAppUpdate.mockResolvedValue({
      ...availableCheck,
      asset: { ...updateAsset, name: portableDownload.fileName }
    });
    backend.downloadAppUpdate.mockResolvedValue(portableDownload);
    backend.installAppUpdate.mockResolvedValue({ action: "revealed" });
    render(<UpdateSettings controller={createAppUpdateController(backend)} />);

    await user.click(await screen.findByRole("button", { name: "下载更新（34.5 MiB）" }));
    expect(await screen.findByRole("button", { name: "在文件夹中显示" })).toBeInTheDocument();
    expect(screen.getByText("便携版需要手动替换：在托盘图标菜单选择「关闭 Mewrk」，把压缩包解压到 C:\\Program Files\\Mewrk 覆盖旧文件，再重新打开。")).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "在文件夹中显示" }));
    await waitFor(() => expect(backend.installAppUpdate).toHaveBeenCalledWith(portableDownload.path));
    expect(screen.getByRole("button", { name: "在文件夹中显示" })).toBeInTheDocument();
  });

  it("offers the release page when no matching asset exists", async () => {
    const backend = createBackend();
    backend.checkAppUpdate.mockResolvedValue({ ...availableCheck, asset: null });
    render(<UpdateSettings controller={createAppUpdateController(backend)} />);

    expect(await screen.findByRole("link", { name: "前往发布页" })).toHaveAttribute(
      "href",
      availableCheck.release.htmlUrl
    );
    expect(screen.queryByRole("button", { name: /下载更新/ })).not.toBeInTheDocument();
  });

  it("checks for the Mac app and sends it to the release page for the next .dmg", async () => {
    const backend = createBackend();
    updateMocks.appVersionInfo.mockResolvedValue({
      ...versionInfo,
      flavor: "mac_app",
      arch: "aarch64",
      os: "macos",
      executableDir: "/Applications/Mewrk.app/Contents/MacOS"
    });
    backend.checkAppUpdate.mockResolvedValue({
      ...availableCheck,
      asset: null,
      checksumsAsset: null,
      inAppInstall: false
    });
    render(<UpdateSettings controller={createAppUpdateController(backend)} />);

    expect(await screen.findByText("发现新版本 v1.1.0")).toBeInTheDocument();
    expect(screen.getByText(/请到发布页下载新的 \.dmg。/)).toBeInTheDocument();
    expect(screen.getByText(/但不负责下载/)).toBeInTheDocument();
    expect(screen.queryByText(/dl\.mewrk\.dev/)).not.toBeInTheDocument();
    expect(screen.queryByText(/便携版/)).not.toBeInTheDocument();
    expect(screen.getByRole("link", { name: "前往发布页" })).toHaveAttribute(
      "href",
      availableCheck.release.htmlUrl
    );
    expect(screen.queryByRole("button", { name: /下载更新/ })).not.toBeInTheDocument();
    expect(backend.downloadAppUpdate).not.toHaveBeenCalled();
  });

  it("checks for an MSIX installed from the release page and points at the next .msix", async () => {
    const backend = createBackend();
    updateMocks.appVersionInfo.mockResolvedValue({ ...versionInfo, flavor: "sideloaded_msix" });
    backend.checkAppUpdate.mockResolvedValue({
      ...availableCheck,
      asset: null,
      checksumsAsset: null,
      inAppInstall: false
    });
    render(<UpdateSettings controller={createAppUpdateController(backend)} />);

    expect(await screen.findByText("发现新版本 v1.1.0")).toBeInTheDocument();
    expect(screen.getByText(/没有任何渠道会自动更新它/)).toBeInTheDocument();
    expect(screen.getByText(/请到发布页下载并安装新的 \.msix。/)).toBeInTheDocument();
    expect(screen.queryByText("由 Microsoft Store 更新")).not.toBeInTheDocument();
    expect(screen.getByRole("link", { name: "前往发布页" })).toBeInTheDocument();
    expect(backend.downloadAppUpdate).not.toHaveBeenCalled();
  });

  it("shows a failed check and retries it on request", async () => {
    const user = userEvent.setup();
    const backend = createBackend();
    backend.checkAppUpdate
      .mockRejectedValueOnce(new Error("GitHub API 的匿名请求配额已用完"))
      .mockResolvedValueOnce(upToDateCheck);
    render(<UpdateSettings controller={createAppUpdateController(backend)} />);

    expect(await screen.findByRole("alert")).toHaveTextContent("GitHub API 的匿名请求配额已用完");
    await user.click(screen.getByRole("button", { name: "重试" }));
    await waitFor(() => expect(backend.checkAppUpdate).toHaveBeenCalledTimes(2));
  });

  describe("AI SDK component row", () => {
    function renderPage() {
      return render(<UpdateSettings controller={createAppUpdateController(createBackend())} />);
    }

    it("reads the component status once when the page mounts and shows the build in use", async () => {
      renderPage();

      expect(await screen.findByText("AI SDK 组件")).toBeInTheDocument();
      expect(screen.getByText(/^v1\.2\.4 · 构建于 2026年10月[67]日$/)).toBeInTheDocument();
      expect(updateMocks.appComponentsStatus).toHaveBeenCalledTimes(1);
    });

    it("says a development build uses the source tree", async () => {
      updateMocks.appComponentsStatus.mockResolvedValue({
        aisdk: { ...installedAisdk, state: "development", version: null, builtAt: null }
      });
      renderPage();

      expect(await screen.findByText("开发版本：使用源码目录里的 AI SDK 进程。")).toBeInTheDocument();
    });

    it("follows a download with a progress bar until it settles", async () => {
      updateMocks.appComponentsStatus
        .mockResolvedValueOnce({
          aisdk: { ...installedAisdk, state: "downloading", receivedBytes: 10_485_760, totalBytes: 41_943_040 }
        })
        .mockResolvedValue({ aisdk: installedAisdk });
      vi.useFakeTimers({ shouldAdvanceTime: true });
      try {
        renderPage();

        expect(await screen.findByText("正在下载 v1.2.4…")).toBeInTheDocument();
        const bar = screen.getByRole("progressbar", { name: "AI SDK 组件下载进度" });
        expect(bar).toHaveAttribute("aria-valuenow", "25");
        expect(screen.getByText("10.0 MiB / 40.0 MiB · 25%")).toBeInTheDocument();

        await act(async () => {
          await vi.advanceTimersByTimeAsync(1000);
        });
        expect(screen.getByText(/^v1\.2\.4 · 构建于/)).toBeInTheDocument();
        expect(screen.queryByRole("progressbar", { name: "AI SDK 组件下载进度" })).not.toBeInTheDocument();
        expect(updateMocks.appComponentsStatus).toHaveBeenCalledTimes(2);
      } finally {
        vi.useRealTimers();
      }
    });

    it("shows a failed update as an alert", async () => {
      updateMocks.appComponentsStatus.mockResolvedValue({
        aisdk: { ...installedAisdk, state: "failed", version: null, builtAt: null, error: "下载镜像不可达" }
      });
      renderPage();

      const alerts = await screen.findAllByText("更新失败：下载镜像不可达");
      expect(alerts[0].closest("[role=alert]")).not.toBeNull();
    });

    it("reports a failed read of the status", async () => {
      updateMocks.appComponentsStatus.mockRejectedValue(new Error("ipc down"));
      renderPage();

      expect(await screen.findByText("读取组件状态失败：ipc down")).toBeInTheDocument();
    });
  });

  it("uses English copy when the locale resolves to English", async () => {
    configureI18n("auto", "en-US");
    const backend = createBackend();
    render(<UpdateSettings controller={createAppUpdateController(backend)} />);

    expect(screen.getByRole("heading", { name: "Updates" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Check for updates" })).toBeInTheDocument();
    expect(await screen.findByText("You are up to date")).toBeInTheDocument();
  });
});
