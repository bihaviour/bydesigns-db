<?php

declare(strict_types=1);

/**
 * Minimal PSR-4 autoloader for the `Twill\` namespace, for use without Composer
 * (tests, examples, single-file scripts). In a real project, prefer Composer's
 * generated autoloader (`vendor/autoload.php`).
 */
\spl_autoload_register(static function (string $class): void {
    $prefix = 'Twill\\';
    if (\strncmp($class, $prefix, \strlen($prefix)) !== 0) {
        return;
    }
    $relative = \substr($class, \strlen($prefix));
    $file = __DIR__ . '/' . \str_replace('\\', '/', $relative) . '.php';
    if (\is_file($file)) {
        require $file;
    }
});
