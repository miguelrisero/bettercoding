import { useCallback, useMemo, useState } from 'react';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { useDropzone } from 'react-dropzone';
import {
  ArrowClockwiseIcon,
  CaretRightIcon,
  CheckIcon,
  DownloadSimpleIcon,
  FileIcon,
  FileZipIcon,
  FolderSimpleIcon,
  HouseIcon,
  LinkIcon,
  SpinnerIcon,
  UploadSimpleIcon,
  WarningIcon,
  XIcon,
} from '@phosphor-icons/react';
import { ApiError, workspaceFilesApi } from '@/shared/lib/api';
import { cn, formatFileSize } from '@/shared/lib/utils';
import type { WorkspaceFileEntry } from 'shared/types';

interface WorkspaceFilesPanelProps {
  workspaceId: string;
}

type UploadTarget = 'drop' | 'current';

// Where uploads land when the "Drop folder" target is selected.
const DROP_FOLDER = '.vibe-uploads';

function joinPath(base: string, name: string): string {
  return base ? `${base}/${name}` : name;
}

export function WorkspaceFilesPanel({ workspaceId }: WorkspaceFilesPanelProps) {
  const queryClient = useQueryClient();
  const [path, setPath] = useState('');
  const [uploadTarget, setUploadTarget] = useState<UploadTarget>('drop');
  const [isUploading, setIsUploading] = useState(false);
  const [uploadError, setUploadError] = useState<string | null>(null);
  // Upload held back after a 409 so the user can confirm an overwrite. The
  // resolved target path is captured so a later target toggle can't redirect it.
  const [conflict, setConflict] = useState<{
    files: File[];
    targetPath: string | undefined;
  } | null>(null);

  const {
    data: listing,
    isLoading,
    isError,
    error,
    refetch,
    isFetching,
  } = useQuery({
    queryKey: ['workspace-files', workspaceId, path],
    queryFn: () => workspaceFilesApi.list(workspaceId, path),
  });

  const crumbs = useMemo(
    () => (path ? path.split('/').filter(Boolean) : []),
    [path]
  );

  const invalidate = useCallback(() => {
    queryClient.invalidateQueries({
      queryKey: ['workspace-files', workspaceId],
    });
  }, [queryClient, workspaceId]);

  const runUpload = useCallback(
    async (
      files: File[],
      targetPath: string | undefined,
      overwrite: boolean
    ) => {
      if (files.length === 0) return;
      setIsUploading(true);
      setUploadError(null);
      try {
        // Empty targetPath => server drops into the .vibe-uploads folder.
        await workspaceFilesApi.upload(workspaceId, files, {
          path: targetPath,
          overwrite,
        });
        setConflict(null);
        invalidate();
      } catch (err) {
        if (err instanceof ApiError && err.status === 409) {
          // Stash files + destination so the confirm retries the same target.
          setConflict({ files, targetPath });
        } else {
          setUploadError(err instanceof Error ? err.message : 'Upload failed');
        }
      } finally {
        setIsUploading(false);
      }
    },
    [workspaceId, invalidate]
  );

  const onDrop = useCallback(
    (accepted: File[]) => {
      if (accepted.length === 0) return;
      const targetPath = uploadTarget === 'current' ? path : undefined;
      runUpload(accepted, targetPath, false);
    },
    [runUpload, uploadTarget, path]
  );

  const { getRootProps, getInputProps, isDragActive, open } = useDropzone({
    onDrop,
    disabled: isUploading,
    noClick: true,
    noKeyboard: true,
  });

  const navigateTo = useCallback((next: string) => {
    setPath(next);
    setUploadError(null);
    setConflict(null);
  }, []);

  const crumbTargetPath = (index: number) =>
    crumbs.slice(0, index + 1).join('/');

  const targetLabel =
    uploadTarget === 'current'
      ? path
        ? `/${path}`
        : 'workspace root'
      : `${DROP_FOLDER}/`;

  return (
    <div
      {...getRootProps()}
      className="relative flex flex-col flex-1 min-h-0 w-full text-base"
    >
      <input {...getInputProps()} />

      {/* Breadcrumbs */}
      <div className="flex items-center gap-half flex-wrap px-base py-half border-b text-low">
        <button
          type="button"
          onClick={() => navigateTo('')}
          className={cn(
            'flex items-center hover:text-normal',
            path === '' && 'text-normal'
          )}
          title="Workspace root"
        >
          <HouseIcon className="size-icon-xs" weight="bold" />
        </button>
        {crumbs.map((crumb, index) => (
          <span key={index} className="flex items-center gap-half min-w-0">
            <CaretRightIcon className="size-icon-xs shrink-0" />
            <button
              type="button"
              onClick={() => navigateTo(crumbTargetPath(index))}
              className={cn(
                'truncate hover:text-normal',
                index === crumbs.length - 1 && 'text-normal'
              )}
              title={crumb}
            >
              {crumb}
            </button>
          </span>
        ))}
        <button
          type="button"
          onClick={() => refetch()}
          className="ml-auto shrink-0 hover:text-normal"
          title="Refresh"
        >
          <ArrowClockwiseIcon
            className={cn('size-icon-xs', isFetching && 'animate-spin')}
            weight="bold"
          />
        </button>
      </div>

      {/* Upload controls */}
      <div className="flex items-center gap-half px-base py-half border-b">
        <div className="flex items-center rounded border overflow-hidden text-xs">
          <button
            type="button"
            onClick={() => setUploadTarget('drop')}
            className={cn(
              'px-base py-half',
              uploadTarget === 'drop'
                ? 'bg-panel text-normal'
                : 'text-low hover:text-normal'
            )}
            title={`Upload into ${DROP_FOLDER}`}
          >
            Drop folder
          </button>
          <button
            type="button"
            onClick={() => setUploadTarget('current')}
            className={cn(
              'px-base py-half border-l',
              uploadTarget === 'current'
                ? 'bg-panel text-normal'
                : 'text-low hover:text-normal'
            )}
            title="Upload into the current folder"
          >
            This folder
          </button>
        </div>
        <button
          type="button"
          onClick={open}
          disabled={isUploading}
          className="ml-auto flex items-center gap-half px-base py-half rounded border text-low hover:text-normal disabled:opacity-50"
          title="Upload files"
        >
          {isUploading ? (
            <SpinnerIcon className="size-icon-xs animate-spin" />
          ) : (
            <UploadSimpleIcon className="size-icon-xs" weight="bold" />
          )}
          <span>Upload</span>
        </button>
      </div>

      {/* Conflict / error banners */}
      {conflict && (
        <div className="flex items-center gap-half px-base py-half border-b bg-panel text-normal">
          <WarningIcon className="size-icon-xs shrink-0 text-brand" />
          <span className="flex-1 truncate">File exists — replace?</span>
          <button
            type="button"
            onClick={() => runUpload(conflict.files, conflict.targetPath, true)}
            disabled={isUploading}
            className="flex items-center gap-half text-brand hover:opacity-80 disabled:opacity-50"
          >
            <CheckIcon className="size-icon-xs" weight="bold" />
            Replace
          </button>
          <button
            type="button"
            onClick={() => setConflict(null)}
            className="flex items-center gap-half text-low hover:text-normal"
          >
            <XIcon className="size-icon-xs" weight="bold" />
            Cancel
          </button>
        </div>
      )}
      {uploadError && (
        <div className="flex items-center gap-half px-base py-half border-b text-error">
          <WarningIcon className="size-icon-xs shrink-0" />
          <span className="flex-1 truncate">{uploadError}</span>
          <button
            type="button"
            onClick={() => setUploadError(null)}
            className="hover:opacity-80"
          >
            <XIcon className="size-icon-xs" weight="bold" />
          </button>
        </div>
      )}

      {/* Listing */}
      <div className="flex-1 min-h-0 overflow-y-auto">
        {isLoading ? (
          <div className="flex items-center justify-center py-double">
            <SpinnerIcon className="size-icon-sm animate-spin text-low" />
          </div>
        ) : isError ? (
          <div className="px-base py-base text-error text-sm">
            {error instanceof Error ? error.message : 'Failed to load files'}
          </div>
        ) : !listing || listing.entries.length === 0 ? (
          <div className="px-base py-base text-low text-sm">
            This folder is empty.
          </div>
        ) : (
          <ul>
            {listing.entries.map((entry) => (
              <FileRow
                key={entry.path}
                workspaceId={workspaceId}
                entry={entry}
                onOpenDir={() => navigateTo(joinPath(path, entry.name))}
              />
            ))}
          </ul>
        )}
        {listing?.truncated && (
          <div className="px-base py-half text-low text-xs">
            Showing first {listing.entries.length} (truncated)
          </div>
        )}
      </div>

      {/* Footer: download current folder as zip */}
      <div className="flex items-center gap-half px-base py-half border-t">
        <a
          href={workspaceFilesApi.downloadZipUrl(workspaceId, path)}
          download
          className="flex items-center gap-half text-low hover:text-normal"
          title="Download this folder as a .zip"
        >
          <FileZipIcon className="size-icon-xs" weight="bold" />
          <span>Download folder as .zip</span>
        </a>
      </div>

      {/* Drag overlay */}
      {isDragActive && (
        <div className="absolute inset-0 z-10 flex items-center justify-center bg-primary/80 border-2 border-dashed border-brand pointer-events-none">
          <span className="text-normal">Drop to upload to {targetLabel}</span>
        </div>
      )}
    </div>
  );
}

interface FileRowProps {
  workspaceId: string;
  entry: WorkspaceFileEntry;
  onOpenDir: () => void;
}

function FileRow({ workspaceId, entry, onOpenDir }: FileRowProps) {
  const Icon = entry.is_dir ? FolderSimpleIcon : FileIcon;

  return (
    <li className="group flex items-center gap-half px-base py-half hover:bg-panel">
      {entry.is_dir ? (
        <button
          type="button"
          onClick={onOpenDir}
          className="flex items-center gap-half min-w-0 flex-1 text-normal"
          title={entry.name}
        >
          <Icon className="size-icon-xs shrink-0 text-low" weight="bold" />
          <span className="truncate">{entry.name}</span>
          {entry.is_symlink && (
            <LinkIcon className="size-icon-xs shrink-0 text-low" />
          )}
        </button>
      ) : (
        <div className="flex items-center gap-half min-w-0 flex-1 text-normal">
          <Icon className="size-icon-xs shrink-0 text-low" weight="bold" />
          <span className="truncate" title={entry.name}>
            {entry.name}
          </span>
          {entry.is_symlink && (
            <LinkIcon className="size-icon-xs shrink-0 text-low" />
          )}
        </div>
      )}

      {!entry.is_dir && (
        <span className="shrink-0 text-low text-xs tabular-nums">
          {formatFileSize(entry.size_bytes)}
        </span>
      )}

      {!entry.is_dir && (
        <a
          href={workspaceFilesApi.downloadUrl(workspaceId, entry.path)}
          download
          onClick={(e) => e.stopPropagation()}
          className="shrink-0 text-low opacity-0 group-hover:opacity-100 group-focus-within:opacity-100 focus-visible:opacity-100 hover:text-normal focus-visible:text-normal"
          title={`Download ${entry.name}`}
        >
          <DownloadSimpleIcon className="size-icon-xs" weight="bold" />
        </a>
      )}
    </li>
  );
}
